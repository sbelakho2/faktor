// Pure Edition 5 view model: destinations, per-control served state, draft
// validation and screen data. No VS Code or DOM imports, so selftest and the
// exercise/acceptance harness drive exactly the same decisions as the panel.

import { E5_CONTROLS, E5_SCREENS, type Edition5Control, type Edition5Screen } from '../generated/edition5Registry.ts';
import { bindingFor, type E5Binding, type E5CommandKind } from './bindings.ts';
import type {
  E5ApplyState,
  E5ConfigSnapshot,
  E5Snapshot,
  Json,
} from './contract.ts';

export type E5Destination = 'work' | 'inspect' | 'history' | 'settings';

export type E5ControlState = 'available' | 'empty' | 'loading' | 'stale' | 'error' | 'disabled';

export interface E5ControlView {
  readonly control: Edition5Control;
  readonly binding: E5Binding;
  readonly state: E5ControlState;
  readonly stateText: string;
  readonly valueText: string;
  readonly disabled: boolean;
  readonly disabledReason: string | null;
  readonly configValue: Json | null;
  readonly applyState: E5ApplyState | null;
  readonly source: string | null;
  readonly options: readonly string[];
}

export interface E5Row {
  readonly key: string;
  readonly label: string;
  readonly value: string;
  readonly state: 'served' | 'unavailable' | 'warning' | 'error';
}

export interface E5ScreenView {
  readonly screen: Edition5Screen;
  readonly rows: readonly E5Row[];
  readonly controls: readonly E5ControlView[];
  readonly stale: boolean;
  readonly staleText: string | null;
  readonly emptyText: string | null;
}

export interface E5PanelState {
  readonly snapshot: E5Snapshot | null;
  readonly loading: boolean;
  readonly error: string | null;
  readonly focusControl: string | null;
}

const SETTINGS_SCREENS = new Set(['S03', 'S04', 'S05', 'S06', 'S07', 'S11', 'S12', 'S13', 'S15', 'S16', 'S17', 'S18', 'S21', 'S28', 'S29', 'S30', 'S31']);

export function destinationOf(screenId: string): E5Destination {
  if (SETTINGS_SCREENS.has(screenId)) return 'settings';
  if (screenId === 'S26' || screenId === 'S27') return 'history';
  if (screenId === 'S01' || screenId === 'S02' || screenId === 'S08' || screenId === 'S09' || screenId === 'S10' || screenId === 'S14' || screenId === 'S22' || screenId === 'S23' || screenId === 'S24' || screenId === 'S25' || screenId === 'S32') {
    return 'work';
  }
  return 'inspect';
}

export function screensFor(destination: E5Destination): readonly Edition5Screen[] {
  return E5_SCREENS.filter((screen) => destinationOf(screen.id) === destination);
}

export function screenById(screenId: string): Edition5Screen {
  const screen = E5_SCREENS.find((entry) => entry.id === screenId);
  if (screen === undefined) throw new Error(`edition5: unknown screen ${screenId}`);
  return screen;
}

export function controlsForScreen(screenId: string): readonly Edition5Control[] {
  const screen = screenById(screenId);
  return screen.controls
    .map((id) => E5_CONTROLS.find((control) => control.id === id))
    .filter((control): control is Edition5Control => control !== undefined);
}

// ---------------------------------------------------------- config matching

function configValueFor(snapshot: E5Snapshot | null, key: string | null): E5ConfigSnapshot['values'][number] | null {
  if (snapshot === null || snapshot.config === null || key === null) return null;
  const exact = snapshot.config.values.find((value) => value.key === key);
  if (exact !== undefined) return exact;
  const bracket = key.lastIndexOf('].');
  if (bracket >= 0) {
    const suffix = key.slice(bracket + 2);
    const candidates = snapshot.config.values.filter((value) => value.key.endsWith(`.${suffix}`));
    if (candidates.length === 1) return candidates[0];
  }
  return null;
}

function jsonText(value: Json | null): string {
  if (value === null || value === undefined) return 'Inherit (not configured)';
  if (typeof value === 'string') return value;
  if (typeof value === 'number' || typeof value === 'boolean') return String(value);
  return JSON.stringify(value);
}

// ------------------------------------------------------------ prerequisites

function unavailableReason(controlId: string, snapshot: E5Snapshot | null): string | null {
  if (snapshot === null) return 'The daemon has not served the Edition 5 state yet.';
  const missing = (what: string) => `Missing prerequisite: ${what}.`;
  switch (controlId) {
    case 'M-01':
    case 'M-02':
      return snapshot.models === null ? missing('the served model catalog (GET /models)') : null;
    case 'M-03':
      return snapshot.models === null || snapshot.models.catalog.length === 0
        ? missing('a selected served model')
        : null;
    case 'M-07':
      return snapshot.models === null ? missing('the served capability authority') : null;
    case 'M-09':
      return snapshot.models === null ? missing('the served model context limit') : null;
    case 'M-11':
      return snapshot.models === null || snapshot.models.calls.length === 0
        ? missing('a durable call receipt (no call yet)')
        : null;
    case 'M-13':
      return snapshot.models?.price === null || snapshot.models?.price === undefined
        ? missing('a served price/limit sheet')
        : null;
    case 'P-08':
    case 'P-09':
      return snapshot.policy === null ? missing('the effective sandbox/network policy') : null;
    case 'P-10':
      return snapshot.providers === null ? missing('a provider connection to test') : null;
    case 'P-13':
      return snapshot.providers?.local === null || snapshot.providers?.local === undefined
        ? missing('a served daemon execution location')
        : null;
    case 'P-15':
      return snapshot.providers?.local?.unload_supported === false
        ? 'The adapter does not advertise context allocation, keep-alive or unload.'
        : null;
    case 'A-10':
      return snapshot.agents === null || snapshot.agents.tree.every((node) => node.parent_id === null)
        ? missing('an active child agent')
        : null;
    case 'C-14':
      return snapshot.context?.manifest === null || snapshot.context?.manifest === undefined
        ? missing('a captured context manifest')
        : null;
    case 'K-03':
      return snapshot.compaction?.plan === null || snapshot.compaction?.plan === undefined
        ? missing('a served compaction plan')
        : null;
    case 'K-11':
      return snapshot.memory === null || snapshot.memory.records.length === 0
        ? missing('a saved memory record')
        : null;
    case 'Q-06':
      return snapshot.approvals === null || snapshot.approvals.pending.length === 0
        ? missing('a live permission waiter')
        : null;
    case 'Q-07':
      return snapshot.approvals === null || !snapshot.approvals.pending.some((request) => request.remember_available)
        ? 'No persistent policy store is served for this request; Remember is unavailable.'
        : null;
    case 'T-04':
      return snapshot.tools?.mcp.every((server) => !server.remote_supported) ?? true
        ? 'Remote transport is not advertised by any MCP capability negotiation.'
        : null;
    case 'T-06':
      return snapshot.tools?.mcp.every((server) => server.auth_state === 'not_required') ?? true
        ? 'No MCP server currently requires OAuth.'
        : null;
    case 'T-13':
      return snapshot.tools === null || snapshot.tools.mcp.length === 0
        ? missing('a configured MCP server')
        : null;
    case 'S-04':
    case 'S-14':
      return snapshot.skills === null ? missing('the skill library') : null;
    case 'S-13':
      return snapshot.skills === null || snapshot.skills.hooks.length === 0
        ? missing('a saved hook version')
        : null;
    case 'X-04':
      return snapshot.terminal === null || snapshot.terminal.processes.length === 0
        ? missing('a session-owned process')
        : null;
    case 'X-05':
      return snapshot.terminal === null || !snapshot.terminal.processes.some((process) => process.live)
        ? missing('a live owned process handle')
        : null;
    case 'X-06':
      return snapshot.browser?.adapters.every((adapter) => !adapter.available) ?? true
        ? 'No browser adapter is advertised; unavailable support is not shown as an idle tab.'
        : null;
    case 'X-08':
      return snapshot.browser === null || snapshot.browser.sessions.length === 0
        ? missing('a connected browser session')
        : null;
    case 'B-08':
      return snapshot.budget?.rate_limit === null || snapshot.budget?.rate_limit === undefined
        ? missing('a typed rate-limit state to cancel')
        : null;
    case 'B-12':
      return snapshot.diagnostics === null || snapshot.diagnostics.uncertain_admissions.length === 0
        ? missing('an uncertain admission receipt')
        : null;
    case 'V-01':
    case 'V-02':
    case 'V-03':
      return snapshot.changes === null || snapshot.changes.files.length === 0
        ? missing('a candidate change')
        : null;
    case 'V-05':
    case 'V-06':
      return snapshot.checkpoints === null || !snapshot.checkpoints.some((checkpoint) => checkpoint.content_available)
        ? missing('a content-aware checkpoint (hash-only entries cannot restore or fork content)')
        : null;
    case 'D-03':
      return snapshot.history === null
        ? missing('a durable history service (not an empty list)')
        : null;
    case 'D-06':
      return snapshot.binding?.session === null || snapshot.binding?.session === undefined
        ? missing('a durable conversation')
        : null;
    case 'O-03':
      return snapshot.diagnostics?.export_preview === null || snapshot.diagnostics?.export_preview === undefined
        ? 'Preview the redacted diagnostic bundle before exporting it.'
        : null;
    case 'O-05':
      return snapshot.diagnostics === null ? missing('a runtime readiness read') : null;
    case 'O-12':
      return snapshot.settings?.import_export?.import_available === false
        ? 'Import is not supported by the served configuration schema.'
        : null;
    case 'O-13':
      return snapshot.diagnostics === null ? missing('a version/capability handshake') : null;
    case 'V-09':
      return snapshot.proof === null ? missing('a served completion basis') : null;
    case 'O-06':
      return snapshot.usage === null ? missing('observed usage and quota') : null;
    case 'M-14':
      return snapshot.models === null ? missing('two served candidate models') : null;
    default:
      return null;
  }
}

export function controlView(
  control: Edition5Control,
  snapshot: E5Snapshot | null,
  loading: boolean,
  error: string | null,
): E5ControlView {
  const binding = bindingFor(control);
  const config = configValueFor(snapshot, binding.configKey);
  const options = optionsFor(binding, snapshot);
  let state: E5ControlState = 'available';
  let stateText = '';
  if (snapshot === null) {
    state = loading ? 'loading' : 'empty';
    stateText = loading
      ? 'Loading the served state; the last coherent view is kept until it arrives.'
      : 'No Edition 5 state is served yet. Refresh names the missing read.';
  } else if (error !== null) {
    state = 'error';
    stateText = error;
  } else if (snapshot.stale) {
    state = 'stale';
    stateText = `Stale by ${ageText(snapshot.stale_age_ms)}; admission and apply stay disabled until the session is reconciled.`;
  }
  const reason = unavailableReason(control.id, snapshot);
  if (reason !== null && (state === 'available' || state === 'stale')) {
    state = 'disabled';
    stateText = reason;
  } else if (config?.locked === true && (state === 'available' || state === 'stale')) {
    state = 'disabled';
    stateText = `Locked by ${config.lock_source ?? config.source}; the served effective value is read-only.`;
  }
  const valueText =
    config !== null
      ? `${jsonText(config.value)} · ${config.source}`
      : binding.input === 'readonly'
        ? 'Read-only served value'
        : 'Inherit (not configured)';
  return {
    control,
    binding,
    state,
    stateText,
    valueText,
    disabled: state === 'disabled' || state === 'error' || state === 'loading',
    disabledReason: state === 'disabled' || state === 'error' ? stateText : null,
    configValue: config === null ? null : config.value,
    applyState: config === null ? null : config.apply_state,
    source: config === null ? null : config.source,
    options,
  };
}

export function optionsFor(binding: E5Binding, snapshot: E5Snapshot | null): readonly string[] {
  if (binding.options.length > 0) return binding.options;
  if (snapshot === null || binding.optionsSource === null) return [];
  switch (binding.optionsSource) {
    case 'models':
      return (snapshot.models?.catalog ?? [])
        .filter((model) => model.availability === 'available')
        .map((model) => `${model.provider}/${model.model}`);
    case 'providers':
      return (snapshot.providers?.connections ?? []).map((connection) => connection.id);
    case 'presets':
      return (snapshot.agents?.presets ?? []).map((preset) => preset.id);
    case 'roles':
      return (snapshot.agents?.roles ?? []).map((role) => role.role);
    case 'hooks':
      return (snapshot.skills?.hooks ?? []).map((hook) => hook.id);
    case 'skills':
      return (snapshot.skills?.skills ?? []).map((skill) => skill.id);
    case 'mcp':
      return (snapshot.tools?.mcp ?? []).map((server) => server.id);
    case 'tools':
      return (snapshot.tools?.builtin ?? []).map((tool) => tool.id);
    case 'processes':
      return (snapshot.terminal?.processes ?? []).map((process) => process.id);
    case 'checkpoints':
      return (snapshot.checkpoints ?? []).map((checkpoint) => checkpoint.id);
    default:
      return [];
  }
}

export function ageText(ms: number | null): string {
  if (ms === null || !Number.isFinite(ms) || ms < 0) return 'an unknown age';
  const seconds = Math.floor(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours}h`;
  return `${Math.floor(hours / 24)}d`;
}

// --------------------------------------------------------------- screen rows

function row(key: string, label: string, value: string | null, state: E5Row['state'] = 'served'): E5Row {
  return {
    key,
    label,
    value: value === null || value.length === 0 ? 'Unavailable' : value,
    state: value === null || value.length === 0 ? 'unavailable' : state,
  };
}

function booleanText(value: boolean | null): string {
  return value === null ? 'Unavailable' : value ? 'Yes' : 'No';
}

function capabilityText(value: boolean | null): string {
  return value === null ? 'Unavailable' : value ? 'supported' : 'unsupported';
}

function modelRows(
  cap: (key: string, label: string, value: string | null, state?: E5Row['state']) => void,
  models: E5Snapshot['models'],
  selectedModel: NonNullable<E5Snapshot['models']>['catalog'][number] | null,
): void {
  cap('routing-mode', 'Routing requested', models?.routing?.requested ?? 'Unavailable');
  cap('routing-actual', 'Actual route', models?.routing?.actual ?? 'Unavailable');
  cap('routing-fallback', 'Fallback chain', models?.routing?.fallback.join(' \u2192 ') ?? 'No added fallback');
  cap('catalog-count', 'Served models', models === null ? 'Unavailable' : String(models.catalog.length));
  for (const [index, model] of (models?.catalog ?? []).entries()) {
    cap(
      `model-entry-${index}`,
      `${model.provider}/${model.model}`,
      `${model.availability}${model.unavailable_reason === null ? '' : ` \u2014 ${model.unavailable_reason}`} \u00b7 authority ${model.capability_authority} \u00b7 gated ${model.capabilities_gated ? 'yes' : 'no'}`,
      model.availability === 'available' ? 'served' : 'unavailable',
    );
  }
  cap('favorites', 'Favorites', models?.favorites.join(', ') || 'None');
  cap('recent', 'Recent', models?.recent.join(', ') || 'None');
  if (selectedModel !== null) {
    cap('model-identity', 'Model', `${selectedModel.provider}/${selectedModel.model}`);
    cap('model-authority', 'Capability authority', selectedModel.capability_authority);
    cap('model-gated', 'Capabilities gated', booleanText(selectedModel.capabilities_gated));
    cap('model-context', 'Context', selectedModel.context === null ? 'Unavailable' : `${selectedModel.context} tokens`);
    cap('model-output', 'Max output', selectedModel.max_output === null ? 'Unavailable' : `${selectedModel.max_output} tokens`);
    cap('model-tools', 'Tools', capabilityText(selectedModel.tools));
    cap('model-parallel-tools', 'Parallel tools', selectedModel.parallel_tools === null ? 'Unavailable' : String(selectedModel.parallel_tools));
    cap('model-reasoning', 'Reasoning', capabilityText(selectedModel.reasoning));
    cap('model-thinking', 'Thinking', capabilityText(selectedModel.thinking));
    cap('model-vision', 'Vision', capabilityText(selectedModel.vision));
    cap('model-documents', 'Documents', capabilityText(selectedModel.documents));
    cap('model-streaming', 'Streaming', capabilityText(selectedModel.streaming));
    cap('model-structured', 'Structured output', capabilityText(selectedModel.structured_output));
    cap('model-source', 'Capability source', selectedModel.source ?? 'Unavailable');
  }
  if (models?.price !== null && models?.price !== undefined) {
    cap('price-list', 'List price', models.price.list);
    cap('price-declared', 'Declared price', models.price.declared);
    cap('price-observed', 'Observed price', models.price.observed);
    cap('price-quota', 'Quota', models.price.quota);
    cap('price-basis', 'Price basis', models.price.basis);
  } else {
    cap('price-list', 'List price', null);
  }
  if (models?.quality !== null && models?.quality !== undefined) {
    cap('quality-declared', 'Declared quality', models.quality.declared === null ? 'Unavailable' : String(models.quality.declared));
    cap('quality-observed', 'Observed quality', models.quality.observed === null ? 'Unavailable (not a benchmark)' : String(models.quality.observed));
    cap('quality-samples', 'Quality samples', models.quality.samples === null ? 'Unavailable' : String(models.quality.samples));
  }
  cap('probe-last', 'Last probe', models?.probe?.last_observed_at_ms === null || models?.probe === null || models?.probe === undefined ? 'No probe yet' : new Date(models.probe.last_observed_at_ms ?? 0).toISOString());
  for (const [index, stage] of (models?.probe?.stages ?? []).entries()) {
    cap(`probe-stage-${index}`, `Probe ${stage.stage}`, `${stage.status}${stage.detail === null ? '' : ` \u2014 ${stage.detail}`}`, stage.status === 'failed' ? 'error' : 'served');
  }
  for (const [index, call] of (models?.calls ?? []).slice(0, 5).entries()) {
    cap(`call-${index}`, `Call ${call.call_id}`, `${call.provider}/${call.model} \u00b7 ${call.role} \u00b7 ${call.route_reason}`);
  }
}

function rowsForScreen(screenId: string, snapshot: E5Snapshot | null): readonly E5Row[] {
  if (snapshot === null) return [];
  const rows: E5Row[] = [];
  const cap = (key: string, label: string, value: string | null, state: E5Row['state'] = 'served') => {
    if (rows.length < 120) rows.push(row(key, label, value, state));
  };
  const models = snapshot.models;
  const selectedModel = models?.catalog.find((entry) => entry.availability === 'available') ?? models?.catalog[0] ?? null;
  switch (screenId) {
    case 'S01':
      if (snapshot.context !== null) {
        if (snapshot.context.usage === null) {
          cap('s01-context', 'Context meter', 'Unavailable (contextUsage=null; no durable runtime meter)');
        } else {
          cap('s01-context', 'Context used', `${snapshot.context.usage.used} / ${snapshot.context.usage.capacity} tokens \u00b7 ${snapshot.context.usage.basis}`);
          cap('s01-context-headroom', 'Context headroom', `${snapshot.context.usage.headroom} tokens`);
        }
      } else {
        cap('s01-context', 'Context meter', null);
      }
      if (snapshot.proof !== null) {
        cap('s01-proof', 'Engineering outcome', snapshot.proof.outcome);
        cap('s01-proof-required', 'Required acceptance', `${snapshot.proof.required_passed}/${snapshot.proof.required_total}`);
      }
      if (snapshot.budget !== null) {
        cap('s01-budget', 'Run budget', `${snapshot.budget.token_cap ?? 'Unavailable'} tokens \u00b7 ${snapshot.budget.cost_cap_micro ?? 'Unavailable'} micro-USD`);
      }
      if (snapshot.run !== null) {
        cap('s01-run', 'Live run', snapshot.run.state);
        for (const item of snapshot.run.timeline.slice(-5)) {
          cap(`s01-run-${item.id}`, item.kind, `${item.state} \u00b7 ${item.detail}`);
        }
        for (const item of snapshot.run.queue) {
          cap(`s01-queue-${item.id}`, 'Queued next', `${item.text} \u00b7 ${item.state}`);
        }
      }
      modelRows(cap, models, selectedModel);
      return rows;
    case 'S02':
      modelRows(cap, models, selectedModel);
      return rows;
    case 'S03':
      for (const connection of snapshot.providers?.connections ?? []) {
        cap(`conn-${connection.id}`, connection.label, `${connection.id} · ${connection.kind} · ${connection.base_url} · credential ${connection.credential_reference ?? 'unbound'} · health ${connection.health}`);
      }
      for (const [index, stage] of (snapshot.providers?.stages ?? []).entries()) {
        cap(`stage-${index}`, `Test stage ${stage.stage}`, `${stage.status}${stage.detail === null ? '' : ` — ${stage.detail}`}`, stage.status === 'failed' ? 'error' : 'served');
      }
      if ((snapshot.providers?.connections.length ?? 0) === 0) cap('empty', 'Connections', 'No connections are served; add a connection to begin.');
      return rows;
    case 'S04': {
      const local = snapshot.providers?.local ?? null;
      if (local === null) {
        cap('local', 'Local runtime', null);
        return rows;
      }
      cap('local-location', 'Execution location', local.execution_location);
      cap('local-daemon', 'Daemon host', local.daemon_host);
      cap('local-endpoint', 'Endpoint', local.endpoint);
      cap('local-class', 'Endpoint class', local.endpoint_class);
      cap('local-loopback', 'Loopback authorized', booleanText(local.loopback_authorized));
      cap('local-lan', 'LAN policy', local.lan_policy);
      cap('local-declared', 'Declared allocation', local.declared_allocation === null ? 'Unavailable' : String(local.declared_allocation));
      cap('local-observed', 'Observed allocation', local.observed_allocation === null ? 'Unavailable' : String(local.observed_allocation));
      cap('local-source', 'Observation source', local.observed_source);
      cap('local-keepalive', 'Keep-alive', local.keep_alive);
      cap('local-unload', 'Unload supported', booleanText(local.unload_supported));
      cap('local-embedding', 'Embedding boundary', local.embedding_boundary);
      return rows;
    }
    case 'S05': {
      const generation = snapshot.generation;
      cap('generation-model', 'Model', generation?.model === null || generation?.model === undefined ? 'No served model' : `${generation.model.provider}/${generation.model.model}`);
      cap('generation-timing', 'Apply timing', generation?.apply_timing ?? 'Unavailable');
      for (const parameter of generation?.parameters ?? []) {
        cap(`param-${parameter.key}`, parameter.key, `${parameter.kind}${parameter.min === null ? '' : ` · min ${parameter.min}`}${parameter.max === null ? '' : ` · max ${parameter.max}`}${parameter.choices.length === 0 ? '' : ` · ${parameter.choices.join('/')}`}${parameter.requires_exact_authority ? ' · exact authority required' : ''}`);
      }
      for (const [index, entry] of (generation?.incompatible ?? []).entries()) {
        cap(`incompatible-${index}`, `Incompatible ${entry.key}`, entry.reason, 'error');
      }
      const requested = generation?.requested ?? null;
      const effective = generation?.effective ?? null;
      if (requested !== null && effective !== null) {
        cap('requested', 'Requested bundle', JSON.stringify(requested));
        cap('effective', 'Effective bundle', JSON.stringify(effective));
      } else {
        cap('preview', 'Effective request preview', 'No preview has been served.');
      }
      return rows;
    }
    case 'S06':
    case 'S07':
      for (const preset of snapshot.agents?.presets ?? []) {
        cap(`preset-${preset.id}`, preset.name, `${preset.kind} · v${preset.version} · ${preset.allowed_tools.length} tools · delegation ${preset.delegation} · children ${preset.maximum_children ?? 'unavailable'} · ${preset.enforced ? 'enforced' : 'not enforced'}${preset.ask_prompt_only ? ' · prompt-only Ask' : ''}`);
        for (const [index, permission] of preset.resolved_permissions.entries()) {
          cap(`preset-${preset.id}-perm-${index}`, 'Resolved permission', permission);
        }
      }
      for (const role of snapshot.agents?.roles ?? []) {
        cap(`role-${role.role}`, `Role ${role.role}`, `${role.inherited ? 'inherits selected model' : `${role.provider ?? 'unavailable'}/${role.model ?? 'unavailable'}`} · authority ${role.capability_authority ?? 'unavailable'}`);
      }
      if ((snapshot.agents?.presets.length ?? 0) === 0) cap('empty', 'Presets', 'No presets are served; add or discover one.');
      return rows;
    case 'S08':
      if (snapshot.context === null) {
        cap('context', 'Context accounting', null);
      } else {
        const usage = snapshot.context.usage;
        if (usage === null) {
          cap('context-meter', 'Context meter', 'Unavailable (contextUsage=null; no durable runtime meter)');
        } else {
          cap('context-capacity', 'Capacity', `${usage.capacity} tokens`);
          cap('context-used', 'Used', `${usage.used} tokens`);
          cap('context-output-reserve', 'Output reserve', `${usage.output_reserve} tokens (not consumed input)`);
          cap('context-safety-reserve', 'Safety reserve', `${usage.safety_reserve} tokens (not consumed input)`);
          cap('context-headroom', 'Headroom', `${usage.headroom} tokens`);
          cap('context-basis', 'Basis', usage.basis);
          for (const category of usage.categories) {
            cap(`context-cat-${category.category}`, category.category, `${category.tokens} tokens · ${category.basis}`);
          }
        }
        cap('context-revision', 'Manifest revision', snapshot.context.manifest?.revision ?? null);
        for (const source of snapshot.context.manifest?.sources ?? []) {
          cap(`source-${source.id}`, source.kind, `${source.uri}${source.range === null ? '' : ` ${source.range}`} · v${source.version ?? 'unavailable'} · ${source.bytes ?? 'unavailable'} bytes · ${source.inclusion_reason}${source.failed_reason === null ? '' : ` · failed: ${source.failed_reason}`}`, source.failed_reason === null ? 'served' : 'error');
        }
        cap('context-overflow', 'Overflow', snapshot.context.overflow === null ? 'No overflow' : `over by ${snapshot.context.overflow.over_by} tokens · ${snapshot.context.overflow.actions.join(', ')}`, snapshot.context.overflow === null ? 'served' : 'warning');
        cap('context-retrieval', 'Retrieval', snapshot.context.retrieval === null ? null : `${snapshot.context.retrieval.strategy}${snapshot.context.retrieval.fallback === null ? '' : ` · fallback ${snapshot.context.retrieval.fallback}`}`);
        cap('context-index', 'Index coverage', snapshot.context.retrieval?.index_coverage === null || snapshot.context.retrieval === null || snapshot.context.retrieval === undefined ? 'Unavailable' : `${Math.round((snapshot.context.retrieval.index_coverage ?? 0) * 100)}% · ${snapshot.context.retrieval.index_fresh ? 'fresh' : 'stale'}`);
      }
      return rows;
    case 'S09':
      for (const source of snapshot.context?.manifest?.sources ?? []) {
        cap(`source-${source.id}`, source.kind, `${source.uri} · ${source.included ? 'included' : 'excluded'}${source.pinned ? ' · pinned' : ''}${source.failed_reason === null ? '' : ` · failed: ${source.failed_reason}`}`, source.failed_reason === null ? 'served' : 'error');
      }
      if ((snapshot.context?.manifest?.sources.length ?? 0) === 0) cap('empty', 'Sources', 'No sources are attached; use the mention picker or Add attachment.');
      return rows;
    case 'S10': {
      const plan = snapshot.compaction?.plan ?? null;
      if (plan === null) {
        cap('compaction-plan', 'Compaction plan', 'No plan is served; preview compaction first.');
      } else {
        cap('compaction-retained', 'Retained facts', plan.retained_facts.join(' · '));
        cap('compaction-model', 'Summary model', plan.summary_model);
        cap('compaction-egress', 'Egress', plan.egress);
        cap('compaction-cost', 'Cost', plan.cost_micro === null ? 'Unavailable' : `${plan.cost_micro} micro-USD`);
        cap('compaction-min', 'Minimum reduction', `${Math.round(plan.minimum_reduction * 100)}%`);
        cap('compaction-pending', 'Pending operations', plan.pending_operations.join(', ') || 'None');
      }
      const operation = snapshot.compaction?.operation ?? null;
      if (operation !== null) {
        cap('compaction-state', 'Compaction result', operation.state);
        cap('compaction-measured', 'Measured before/after', operation.measured_before === null || operation.measured_after === null ? 'Unavailable (no measured result)' : `${operation.measured_before} → ${operation.measured_after} tokens`);
        cap('compaction-reduction', 'Measured reduction', operation.reduction_fraction === null ? 'Unavailable' : `${Math.round(operation.reduction_fraction * 100)}%`);
        cap('compaction-version', 'Summary version', operation.summary_version);
        if (operation.error !== null) cap('compaction-error', 'Failure', operation.error, 'error');
      }
      return rows;
    }
    case 'S11':
      for (const source of snapshot.instructions?.sources ?? []) {
        cap(`instruction-${source.id}`, source.origin, `${source.scope} · ${source.path ?? 'no path'} · ${source.enabled ? 'enabled' : 'disabled'}${source.locked ? ` · locked by ${source.lock_source ?? 'managed policy'}` : ''} · v${source.version ?? 'unavailable'}`);
      }
      for (const conflict of snapshot.instructions?.conflicts ?? []) {
        cap(`conflict-${conflict.sources.join('-')}`, 'Conflict', `${conflict.sources.join(' / ')} — ${conflict.detail}`, 'warning');
      }
      cap('instruction-effective-version', 'Effective bundle version', snapshot.instructions?.effective?.version ?? 'Unavailable');
      cap('instruction-effective-hash', 'Effective bundle hash', snapshot.instructions?.effective?.hash ?? 'Unavailable');
      cap('instruction-order', 'Effective order', snapshot.instructions?.effective?.order.join(' → ') ?? 'Unavailable');
      return rows;
    case 'S12':
      for (const record of snapshot.memory?.records ?? []) {
        cap(`memory-${record.id}`, `${record.scope} memory`, `${record.text} · source ${record.source} · ${record.stopped_future_use_at_revision === null ? 'active' : `stopped for future use at ${record.stopped_future_use_at_revision}; prior artifacts retained: ${record.retained_prior_artifacts.join(', ') || 'none'}`}`);
      }
      cap('memory-proposal', 'Proposed memory diff', snapshot.memory?.proposal ?? 'No proposal');
      cap('memory-retention', 'Retention dry run', snapshot.memory?.retention === null || snapshot.memory?.retention === undefined ? 'Unavailable' : snapshot.memory.retention.dry_run.join(', ') || 'Nothing to delete');
      if ((snapshot.memory?.records.length ?? 0) === 0) cap('empty', 'Memory', 'No memory records are served.');
      return rows;
    case 'S13':
      cap('policy-preset', 'Permission preset', snapshot.policy?.preset ?? null);
      cap('policy-filesystem', 'Filesystem', snapshot.policy?.filesystem ?? null);
      cap('policy-shell', 'Shell', snapshot.policy?.shell ?? null);
      cap('policy-network', 'Network', snapshot.policy?.network ?? null);
      cap('policy-guarantee', 'Network guarantee', snapshot.policy?.network_guarantee ?? null);
      cap('policy-sandbox', 'Sandbox status', snapshot.policy === null ? null : `${snapshot.policy.sandbox_state}${snapshot.policy.sandbox_error === null ? '' : ` — ${snapshot.policy.sandbox_error}`}`, snapshot.policy?.sandbox_error === null || snapshot.policy === null ? 'served' : 'error');
      cap('policy-managed-denies', 'Managed denies', snapshot.policy?.managed_denies.join(', ') || 'None');
      for (const rule of snapshot.policy?.rules ?? []) {
        cap(`rule-${rule.id}`, `${rule.tool}/${rule.action}`, `${rule.decision} · ${rule.target} · ${rule.scope} · v${rule.version}${rule.managed ? ' · managed' : ''}`);
      }
      for (const row of snapshot.policy?.audit ?? []) {
        cap(`audit-${row.request_id}-${row.at_ms}`, 'Decision', `${row.tool} → ${row.decision} · ${row.detail}`);
      }
      if (snapshot.policy?.simulation !== null && snapshot.policy?.simulation !== undefined) {
        const simulation = snapshot.policy.simulation;
        cap('policy-simulation', 'Rule match preview', `${simulation.decision} · normalized ${simulation.normalized ?? 'unavailable'} · ${simulation.reasons.join('; ')}${simulation.matched_rule === null ? '' : ` · rule ${simulation.matched_rule}`}`);
      }
      return rows;
    case 'S14':
      for (const request of snapshot.approvals?.pending ?? []) {
        cap(`approval-${request.request_id}`, `${request.owner}${request.child === null ? '' : ` / ${request.child}`}`, `${request.tool} · gen ${request.generation ?? 'unavailable'} · ${request.command ?? request.target ?? 'target unavailable'} · effect ${request.effect_boundary.join(', ')} · expires ${request.expires_at_ms === null ? 'unknown' : new Date(request.expires_at_ms).toISOString()}`);
      }
      for (const row of snapshot.approvals?.history ?? []) {
        cap(`approval-history-${row.request_id}`, 'Reply', `${row.decision} · dispatched ${row.dispatched ? 'yes' : 'no'} · ${row.detail ?? 'no detail'}`);
      }
      if ((snapshot.approvals?.pending.length ?? 0) === 0) cap('empty', 'Approvals', 'No approval waiter is pending.');
      return rows;
    case 'S15':
    case 'S16':
      for (const tool of snapshot.tools?.builtin ?? []) {
        cap(`tool-${tool.id}`, tool.name, `${tool.origin}${tool.server === null ? '' : ` · ${tool.server}`} · ${tool.effect_class} · discovered ${booleanText(tool.discovered)} · enabled ${booleanText(tool.enabled)} · healthy ${booleanText(tool.healthy)} · authorized ${booleanText(tool.authorized)} · ${tool.permission}`);
      }
      for (const server of snapshot.tools?.mcp ?? []) {
        cap(`mcp-${server.id}`, server.name, `${server.transport} · ${server.command} ${server.args.join(' ')} · health ${server.health} · auth ${server.auth_state} · ${server.tool_count} tools · trusted ${booleanText(server.trusted)}${server.error === null ? '' : ` · ${server.error}`}`);
        cap(`mcp-${server.id}-bounds`, `${server.name} bounds`, `timeout ${server.timeout_ms ?? 'unavailable'} ms · stdout ${server.stdout_cap ?? 'unavailable'} bytes · stderr ${server.stderr_cap ?? 'unavailable'} bytes`);
        for (const [index, log] of server.logs.slice(0, 5).entries()) {
          cap(`mcp-${server.id}-log-${index}`, 'Server log', log);
        }
      }
      if ((snapshot.tools?.mcp.length ?? 0) === 0) cap('empty', 'MCP servers', 'No MCP servers are configured; add a trusted stdio server.');
      return rows;
    case 'S17':
      for (const skill of snapshot.skills?.skills ?? []) {
        cap(`skill-${skill.id}`, skill.name, `v${skill.version} · ${skill.source} · trust ${skill.trust} · invocation ${skill.invocation} · capabilities ${skill.capabilities.join(', ') || 'none'}`);
      }
      for (const command of snapshot.skills?.commands ?? []) {
        cap(`command-${command.name}`, command.name, `${command.scope} · ${command.effect} · variables ${command.variables.join(', ') || 'none'}`);
      }
      if ((snapshot.skills?.skills.length ?? 0) === 0) cap('empty', 'Skills', 'No trusted skills are installed.');
      return rows;
    case 'S18':
      cap('hook-events', 'Native hook events', snapshot.skills?.hook_events.join(', ') ?? null);
      for (const hook of snapshot.skills?.hooks ?? []) {
        cap(`hook-${hook.id}`, hook.event, `${hook.executable} ${hook.args.join(' ')} · deadline ${hook.deadline_ms} ms · caps ${hook.stdout_cap}/${hook.stderr_cap} bytes · ${hook.failure_policy} · v${hook.version}`);
        if (hook.last_test !== null) {
          const verdictApplies = hook.last_test.outcome === 'Verdict' && hook.last_test.verdict !== null;
          cap(
            `hook-${hook.id}-test`,
            'Last test',
            `${hook.last_test.outcome}${verdictApplies ? ` · ${hook.last_test.verdict}` : ' · no verdict (the outcome cannot authorize)'} · exit ${hook.last_test.exit_state ?? 'unavailable'} · artifact ${hook.last_test.audit_ref ?? 'unavailable'}`,
            hook.last_test.outcome === 'TimedOut' || hook.last_test.outcome === 'Malformed' || hook.last_test.outcome === 'NonZero' || hook.last_test.outcome === 'Cancelled' || hook.last_test.outcome === 'EffectUnknown' ? 'warning' : 'served',
          );
        }
      }
      if ((snapshot.skills?.hooks.length ?? 0) === 0) cap('empty', 'Hooks', 'No hooks are configured for a native lifecycle event.');
      return rows;
    case 'S19':
      for (const process of snapshot.terminal?.processes ?? []) {
        cap(`process-${process.id}`, `Process ${process.id}`, `${process.live ? 'live' : 'stale'} · pid ${process.pid ?? 'unavailable'} · ${process.state} · ${process.command} · cwd ${process.cwd} · ${process.sandbox_profile} · out ${process.stdout_bytes}/${process.stderr_bytes} bytes · handle gen ${process.handle_generation ?? 'unavailable'}${process.effect_status === null ? '' : ` · effect ${process.effect_status}`}`);
        cap(`process-${process.id}-head`, 'Head', process.output_head);
        cap(`process-${process.id}-tail`, 'Tail', process.output_tail);
        cap(`process-${process.id}-artifact`, 'Full artifact', process.artifact_ref);
      }
      for (const profile of snapshot.terminal?.profiles ?? []) {
        cap(`profile-${profile.name}`, profile.name, profile.detail);
      }
      if ((snapshot.terminal?.processes.length ?? 0) === 0) cap('empty', 'Processes', 'No session-owned processes.');
      return rows;
    case 'S20':
      for (const adapter of snapshot.browser?.adapters ?? []) {
        cap(`browser-${adapter.id}`, adapter.name, `${adapter.available ? 'available' : `unavailable — ${adapter.unavailable_reason ?? 'no reason served'}`} · account ${adapter.account ?? 'unavailable'} · machine ${adapter.machine ?? 'unavailable'}`, adapter.available ? 'served' : 'unavailable');
      }
      for (const session of snapshot.browser?.sessions ?? []) {
        cap(`browser-session-${session.id}`, session.url, `network ${session.network_scope} · screenshot ${session.screenshot_ref ?? 'none'} · console ${session.console.length} lines · network ${session.network.length} lines`);
      }
      if ((snapshot.browser?.adapters.length ?? 0) === 0) cap('empty', 'Browser adapters', 'No browser adapter is advertised.');
      return rows;
    case 'S21':
      cap('budget-tokens', 'Token cap', snapshot.budget?.token_cap === null || snapshot.budget === null || snapshot.budget === undefined ? 'Unavailable' : String(snapshot.budget.token_cap));
      cap('budget-cost', 'Cost cap', snapshot.budget?.cost_cap_micro === null || snapshot.budget === null || snapshot.budget === undefined ? 'Unavailable' : `${snapshot.budget.cost_cap_micro} micro-USD`);
      cap('budget-ledger', 'Ledger', snapshot.budget?.ledger === null || snapshot.budget === undefined || snapshot.budget === null ? 'Unavailable' : `open ${snapshot.budget.ledger.open_micro} · settled ${snapshot.budget.ledger.settled_micro} · refunded ${snapshot.budget.ledger.refunded_micro} · uncertain ${snapshot.budget.ledger.uncertain_micro} micro-USD`);
      cap('budget-warning', 'Cap warning', snapshot.budget?.warning ?? 'None', snapshot.budget?.warning === null || snapshot.budget === undefined || snapshot.budget === null ? 'served' : 'warning');
      cap('budget-enforced', 'Enforcement', snapshot.budget?.enforced === true ? 'Hard refusal enforced' : 'Not enforced');
      if (snapshot.budget?.rate_limit !== null && snapshot.budget?.rate_limit !== undefined) {
        cap('budget-rate', 'Rate limit', `${snapshot.budget.rate_limit.cause} · attempt ${snapshot.budget.rate_limit.attempt}/${snapshot.budget.rate_limit.max_attempts} · retry ${snapshot.budget.rate_limit.retry_at_ms === null ? 'reset unknown' : new Date(snapshot.budget.rate_limit.retry_at_ms).toISOString()}`, 'warning');
      }
      return rows;
    case 'S22':
      cap('run-state', 'Run state', snapshot.run?.state ?? null);
      cap('run-display', 'Display', snapshot.run?.display ?? null);
      for (const [index, item] of (snapshot.run?.timeline ?? []).slice(0, 40).entries()) {
        cap(`run-${index}-${item.id}`, item.kind, `${item.state} · ${item.detail}${item.effect_status === null ? '' : ` · effect ${item.effect_status}`}${item.call === null ? '' : ` · ${item.call.provider}/${item.call.model} (${item.call.role}) route ${item.call.route_reason}`}`, item.effect_status === 'unknown' ? 'warning' : 'served');
      }
      for (const item of snapshot.run?.queue ?? []) {
        cap(`queue-${item.id}`, 'Queued next', `${item.text} · ${item.state}`);
      }
      for (const [index, attention] of (snapshot.run?.attention ?? []).entries()) {
        cap(`attention-${index}`, 'Attention', attention, 'warning');
      }
      if (snapshot.run?.stale === true) cap('run-stale', 'Live projection', 'Stale; reconnect from the durable cursor before enabling duplicate effects.', 'warning');
      return rows;
    case 'S23':
      cap('concurrency', 'Concurrency', snapshot.agents?.concurrency === null || snapshot.agents?.concurrency === undefined ? 'Unavailable' : `${snapshot.agents.concurrency.active}/${snapshot.agents.concurrency.limit ?? 'unavailable'} · shared budget ${snapshot.agents.concurrency.shared_budget_micro ?? 'unavailable'} micro-USD`);
      for (const node of snapshot.agents?.tree ?? []) {
        cap(`agent-${node.agent_id}`, `${node.role}${node.parent_id === null ? ' (parent)' : ` under ${node.parent_id}`}`, `${node.state} · ${node.provider ?? 'unavailable'}/${node.model ?? 'unavailable'} · authority ${node.capability_authority ?? 'unavailable'} · ownership ${node.ownership} · gen ${node.generation ?? 'unavailable'}${node.budget === null ? '' : ` · budget ${node.budget.open_micro}/${node.budget.settled_micro}/${node.budget.uncertain_micro}`}${node.handoff === null ? '' : ` · handoff ${node.handoff}`}`);
      }
      if ((snapshot.agents?.tree.length ?? 0) === 0) cap('empty', 'Agents', 'No parent or child agents are served.');
      return rows;
    case 'S24':
      cap('changes-candidate', 'Candidate', snapshot.changes?.candidate ?? null);
      cap('changes-base', 'Base', snapshot.changes?.base ?? null);
      for (const file of snapshot.changes?.files ?? []) {
        cap(`change-${file.path}`, file.path, `${file.status} · +${file.additions}/-${file.deletions} · hunks ${file.hunks.join(', ') || 'unavailable'}`);
      }
      for (const comment of snapshot.changes?.comments ?? []) {
        cap(`comment-${comment.id}`, `${comment.path}:${comment.line}`, `${comment.body} · v${comment.version}`);
      }
      if (snapshot.changes?.manual_edit_conflict !== null && snapshot.changes?.manual_edit_conflict !== undefined) {
        cap('changes-conflict', 'Manual-edit conflict', snapshot.changes.manual_edit_conflict, 'error');
      }
      for (const [index, criterion] of (snapshot.changes?.acceptance ?? []).entries()) {
        cap(`acceptance-${index}`, 'Acceptance criterion', criterion);
      }
      if ((snapshot.changes?.files.length ?? 0) === 0) cap('empty', 'Changes', 'No candidate changes are served.');
      return rows;
    case 'S25':
      for (const checkpoint of snapshot.checkpoints ?? []) {
        cap(`checkpoint-${checkpoint.id}`, new Date(checkpoint.created_at_ms).toISOString(), `${checkpoint.coverage} · content ${checkpoint.content_available ? 'available' : 'unavailable'} · redo ${checkpoint.redo_supported ? 'supported' : 'unavailable'} · diff ${checkpoint.diff_supported ? 'supported' : 'unavailable'} · files ${checkpoint.files.length} · conversation ${checkpoint.conversation_range ?? 'unavailable'} · uncovered ${checkpoint.uncovered_effects.join(', ') || 'none'}`);
      }
      if ((snapshot.checkpoints?.length ?? 0) === 0) cap('empty', 'Checkpoints', 'No checkpoints are recorded for this conversation.');
      return rows;
    case 'S26':
      cap('history-total', 'Conversations', snapshot.history === null ? 'Unavailable (history service not served; not an empty list)' : `${snapshot.history.sessions.length} served${snapshot.history.total === null ? '' : ` of ${snapshot.history.total}`}`);
      for (const session of snapshot.history?.sessions ?? []) {
        cap(`history-${session.id}`, session.title, `${session.state} · ${session.provider ?? 'unavailable'}/${session.model ?? 'unavailable'} · worktree ${session.worktree ?? 'unavailable'} · outcome ${session.outcome ?? 'unavailable'}${session.pinned ? ' · pinned' : ''}${session.archived ? ' · archived (not deleted)' : ''}${session.active_run ? ' · active run' : ''}`);
      }
      if (snapshot.history?.search !== null && snapshot.history?.search !== undefined) {
        cap('history-search', `Transcript search “${snapshot.history.search.query}”`, `${snapshot.history.search.mode} · ${snapshot.history.search.results.length} results · next ${snapshot.history.search.next_cursor ?? 'none'}`);
        for (const [index, result] of snapshot.history.search.results.slice(0, 20).entries()) {
          cap(`search-${index}`, `${result.session}#${result.seq}`, result.excerpt);
        }
      }
      return rows;
    case 'S27':
      cap('usage-vcsc', 'Verified cost per landed change', snapshot.usage === null ? 'Unavailable' : `${snapshot.usage.vcsc_state}${snapshot.usage.vcsc_micro === null ? '' : ` · ${snapshot.usage.vcsc_micro} micro-USD`}`);
      cap('usage-numerator', 'Eligible attempted spend', snapshot.usage?.attempted_spend_micro ?? null);
      cap('usage-denominator', 'Verified landed changes', snapshot.usage?.verified_landed_changes === null || snapshot.usage === null || snapshot.usage === undefined ? 'Unavailable' : String(snapshot.usage.verified_landed_changes));
      cap('usage-cohort', 'Cohort / period', snapshot.usage === null ? null : `${snapshot.usage.cohort} · ${snapshot.usage.period} · ${snapshot.usage.denominator}`);
      cap('usage-tokens', 'Tokens in / out', snapshot.usage === null ? null : `${snapshot.usage.tokens_in ?? 'Unavailable'} / ${snapshot.usage.tokens_out ?? 'Unavailable'}`);
      cap('usage-prices', 'Provider quota / local estimate / observed', snapshot.usage === null ? null : `${snapshot.usage.provider_quota ?? 'Unavailable'} · ${snapshot.usage.local_estimate ?? 'Unavailable'} · ${snapshot.usage.observed_charge ?? 'Unavailable'}`);
      cap('outcomes', 'Engineering outcomes', snapshot.outcomes === null ? 'Unavailable' : `verified ${snapshot.outcomes.verified} · unverified ${snapshot.outcomes.unverified} · pending ${snapshot.outcomes.pending}`);
      for (const receipt of snapshot.outcomes?.receipts ?? []) {
        cap(`outcome-${receipt.id}`, `Receipt ${receipt.id}`, `${receipt.candidate} · landed ${receipt.landed ? 'yes' : 'no'} · ${receipt.detail}`);
      }
      return rows;
    case 'S28':
      cap('efficiency-retrieval', 'Retrieval', snapshot.efficiency?.retrieval ?? null);
      cap('efficiency-fallback', 'Retrieval fallback', snapshot.efficiency?.fallback ?? 'None');
      for (const flag of snapshot.efficiency?.flags ?? []) {
        cap(`efficiency-${flag.key}`, flag.key, `${flag.enabled ? 'on' : 'off'} · ${flag.provenance}${flag.measured === null ? ' · no measured effect' : ` · ${flag.measured}`}${flag.sample_count === null ? '' : ` · ${flag.sample_count} samples`}`);
      }
      cap('efficiency-aliases', 'Aliases', snapshot.efficiency?.aliases.join(', ') ?? null);
      return rows;
    case 'S29':
      for (const entry of snapshot.settings?.search ?? []) {
        cap(`setting-${entry.key}`, entry.key, `${jsonText(entry.effective)} · ${entry.source} · ${entry.scope}${entry.locked ? ' · locked' : ''}`);
      }
      cap('settings-revision', 'Config revision', snapshot.config?.revision ?? null);
      cap('settings-runtime', 'Runtime revision', snapshot.config?.runtime_revision ?? 'Unavailable');
      cap('settings-import-export', 'Import / export', snapshot.settings?.import_export === null || snapshot.settings?.import_export === undefined ? 'Unavailable' : `${snapshot.settings.import_export.import_available ? 'import' : 'import unavailable'} / ${snapshot.settings.import_export.export_available ? 'export' : 'export unavailable'} · ${snapshot.settings.import_export.schema}`);
      cap('settings-telemetry', 'Telemetry', snapshot.settings?.telemetry === null || snapshot.settings?.telemetry === undefined ? 'Unavailable' : `${snapshot.settings.telemetry.enabled ? 'on' : 'off'} · ${snapshot.settings.telemetry.categories.join(', ')}`);
      return rows;
    case 'S30': {
      const diagnostics = snapshot.diagnostics;
      cap('diag-readiness', 'Readiness', diagnostics === null ? null : `${diagnostics.readiness}${diagnostics.readiness_detail === null ? '' : ` — ${diagnostics.readiness_detail}`}`);
      cap('diag-version', 'Runtime version', diagnostics?.version ?? null);
      cap('diag-path', 'Runtime path', diagnostics?.runtime_path ?? null);
      cap('diag-checksum', 'Checksum', diagnostics === null ? null : `${diagnostics.checksum ?? 'unavailable'} · ${diagnostics.checksum_verified ? 'matches record' : 'not verified'}`);
      cap('diag-signature', 'Publisher signature', diagnostics === null ? null : diagnostics.signature_verified ? 'verified' : 'not verified (a checksum match is not a signature verdict)');
      cap('diag-trust', 'Trust', diagnostics?.trust ?? null);
      cap('diag-protocol', 'Protocol', diagnostics === null ? null : `${diagnostics.protocol_version ?? 'unavailable'} · ${diagnostics.protocol_compatible ? 'compatible' : 'incompatible'}`);
      if (diagnostics?.connectivity !== null && diagnostics?.connectivity !== undefined) {
        cap('diag-connectivity', 'Connectivity', `${diagnostics.connectivity.state} · ${diagnostics.connectivity.detail ?? 'no detail'} · ${diagnostics.connectivity.retryable ? 'retryable' : 'not retryable'}`, 'warning');
      }
      for (const admission of diagnostics?.uncertain_admissions ?? []) {
        cap(`diag-uncertain-${admission.operation_id}`, 'Uncertain admission', `${admission.operation_id} · ${admission.effect_status} · receipt ${admission.receipt ?? 'unavailable'} · retry ${admission.retry_eligible ? 'eligible' : 'not eligible'}`, 'warning');
      }
      if (diagnostics?.export_preview !== null && diagnostics?.export_preview !== undefined) {
        cap('diag-export', 'Export preview', `${diagnostics.export_preview.categories.join(', ')} · omits ${diagnostics.export_preview.omitted.join(', ') || 'nothing'} · ${diagnostics.export_preview.bytes} bytes`);
      }
      for (const [index, line] of (diagnostics?.log_tail ?? []).entries()) {
        cap(`diag-log-${index}`, 'Log', line);
      }
      return rows;
    }
    case 'S31': {
      const binding = snapshot.binding_view;
      cap('binding-editor', 'Editor host', binding?.editor_host ?? null);
      cap('binding-workspace', 'Workspace URI', binding?.workspace_uri ?? null);
      cap('binding-daemon', 'Daemon / shell host', binding?.daemon_host ?? null);
      cap('binding-model', 'Model endpoint', binding?.model_endpoint ?? null);
      cap('binding-embedding', 'Embedding endpoint', binding?.embedding_endpoint ?? null);
      cap('binding-vault', 'Vault location', binding?.vault_location ?? null);
      cap('binding-worker', 'Worker location', binding?.worker_location ?? null);
      cap('binding-boundary', 'Data boundary', binding?.data_boundary ?? null);
      cap('binding-generation', 'Binding generation', binding?.generation === null || binding === null || binding === undefined ? null : String(binding.generation));
      for (const [index, handle] of (binding?.stale_handles ?? []).entries()) {
        cap(`binding-stale-${index}`, 'Stale handle', handle, 'warning');
      }
      return rows;
    }
    case 'S32': {
      const proof = snapshot.proof;
      cap('proof-outcome', 'Engineering outcome', proof === null ? 'Unavailable' : proof.outcome);
      cap('proof-required', 'Required acceptance', proof === null ? null : `${proof.required_passed}/${proof.required_total}`);
      cap('proof-claims', 'Claims complete', proof?.claims_complete === null || proof === null || proof === undefined ? 'Unavailable' : booleanText(proof.claims_complete));
      cap('proof-candidate', 'Candidate', proof?.candidate ?? null);
      cap('proof-verified', 'Verified candidate', proof?.verified_candidate ?? null);
      cap('proof-landed', 'Landed candidate', proof?.landed_candidate ?? null);
      cap('proof-finish', 'Requested finish', proof?.finish_requested ?? null);
      cap('proof-receipt', 'Finish receipt', proof?.finish_receipt ?? 'Unavailable');
      for (const criterion of proof?.criteria ?? []) {
        cap(`proof-criterion-${criterion.key}`, criterion.key, `${criterion.verdict} · ${criterion.requirement} · ${criterion.binding} · ${criterion.detail}`, criterion.verdict === 'unavailable' ? 'warning' : criterion.verdict === 'fail' ? 'error' : 'served');
      }
      for (const [index, blocker] of (proof?.blockers ?? []).entries()) {
        cap(`proof-blocker-${index}`, 'Blocker', blocker, 'error');
      }
      for (const receipt of proof?.receipts ?? []) {
        cap(`proof-receipt-${receipt.id}`, receipt.label, `${receipt.kind} · ${receipt.id}`);
      }
      return rows;
    }
    default:
      return rows;
  }
}

export function screenView(screenId: string, state: E5PanelState): E5ScreenView {
  const screen = screenById(screenId);
  const snapshot = state.snapshot;
  const stale = snapshot?.stale === true;
  let emptyText: string | null = null;
  if (snapshot === null && !state.loading) {
    emptyText = 'No served Edition 5 state. Refresh the daemon read; no placeholder values are shown.';
  }
  return {
    screen,
    rows: rowsForScreen(screenId, snapshot),
    controls: controlsForScreen(screenId).map((control) =>
      controlView(control, snapshot, state.loading, state.error),
    ),
    stale,
    staleText: stale ? `Observations are stale by ${ageText(snapshot.stale_age_ms)}; admission and apply are disabled until reconciled.` : null,
    emptyText,
  };
}

// -------------------------------------------------------------- draft edits

export interface E5StagedChange {
  readonly controlId: string;
  readonly key: string;
  readonly value: Json;
}

export interface E5Draft {
  readonly set: ReadonlyMap<string, E5StagedChange>;
  readonly unset: ReadonlySet<string>;
}

export const EMPTY_DRAFT: E5Draft = { set: new Map(), unset: new Set() };

export interface E5StageResult {
  readonly draft: E5Draft;
  readonly error: string | null;
}

export function parseMoneyMicro(raw: string): { readonly value: string | null; readonly error: string | null } {
  const text = raw.trim();
  if (!/^\d+(\.\d{1,6})?$/.test(text)) {
    return { value: null, error: 'Enter an exact non-negative amount with at most 6 decimals (micro-USD); fractions beyond micro precision are refused.' };
  }
  const [whole, fraction = ''] = text.split('.');
  let micro: bigint;
  try {
    micro = BigInt(whole) * 1_000_000n + BigInt(fraction.padEnd(6, '0'));
  } catch {
    return { value: null, error: 'The amount is not a valid integer micro-USD value.' };
  }
  if (micro > 9_223_372_036_854_775_807n) {
    return { value: null, error: 'The amount exceeds the signed 64-bit micro-USD range.' };
  }
  return { value: micro.toString(), error: null };
}

export function parseControlValue(binding: E5Binding, raw: string | boolean | number): { readonly value: Json; readonly error: string | null } {
  if (binding.input === 'toggle') {
    return { value: raw === true || raw === 'true', error: null };
  }
  const text = typeof raw === 'string' ? raw : String(raw);
  switch (binding.input) {
    case 'number': {
      if (!/^\d+$/.test(text.trim())) return { value: null, error: 'Enter a non-negative integer.' };
      const value = Number(text.trim());
      if (!Number.isSafeInteger(value)) return { value: null, error: 'The integer exceeds the safe JSON range.' };
      if (binding.min !== null && value < binding.min) return { value: null, error: `Minimum is ${binding.min}.` };
      if (binding.max !== null && value > binding.max) return { value: null, error: `Maximum is ${binding.max}.` };
      return { value, error: null };
    }
    case 'money': {
      const parsed = parseMoneyMicro(text);
      return { value: parsed.value, error: parsed.error };
    }
    case 'list': {
      const items = text
        .split(/\r?\n/)
        .map((entry) => entry.trim())
        .filter((entry) => entry.length > 0);
      if (items.length === 0) return { value: [], error: null };
      if (items.length > 64) return { value: null, error: 'At most 64 list entries are accepted.' };
      if (items.some((entry) => entry.length > 512)) return { value: null, error: 'Each list entry must be at most 512 characters.' };
      return { value: items, error: null };
    }
    case 'secret-ref': {
      const reference = text.trim();
      if (!/^(env|vault|keychain|secretref):[A-Za-z0-9._/-]{1,240}$/.test(reference)) {
        return { value: null, error: 'Bind a credential reference (env:NAME, vault:NAME, keychain:NAME); never enter a secret value.' };
      }
      return { value: reference, error: null };
    }
    case 'textarea':
      if (text.length > 64 * 1024) return { value: null, error: 'The text exceeds 64 KiB.' };
      return { value: text, error: null };
    case 'select':
      if (text.length === 0) return { value: null, error: 'Choose a served value.' };
      if (binding.options.length > 0 && !binding.options.includes(text)) {
        return { value: null, error: 'The value is not in the served choice set.' };
      }
      return { value: text, error: null };
    case 'readonly':
    case 'action':
      return { value: null, error: 'This control is read-only; use its action.' };
    default: {
      if (text.length > 4096) return { value: null, error: 'The value exceeds 4 KiB.' };
      if (text.trim().length === 0) return { value: null, error: 'Enter a value or reset the override.' };
      return { value: text, error: null };
    }
  }
}

export function stageControl(draft: E5Draft, binding: E5Binding, raw: string | boolean | number): E5StageResult {
  if (binding.configKey === null) {
    return { draft, error: 'This control does not write a configuration key; use its action.' };
  }
  const parsed = parseControlValue(binding, raw);
  if (parsed.error !== null) return { draft, error: parsed.error };
  const set = new Map(draft.set);
  const unset = new Set(draft.unset);
  set.set(binding.controlId, { controlId: binding.controlId, key: binding.configKey, value: parsed.value });
  unset.delete(binding.controlId);
  return { draft: { set, unset }, error: null };
}

export function unstageControl(draft: E5Draft, binding: E5Binding): E5Draft {
  if (binding.configKey === null) return draft;
  const set = new Map(draft.set);
  set.delete(binding.controlId);
  const unset = new Set(draft.unset);
  unset.add(binding.controlId);
  return { set, unset };
}

export function resetDraft(): E5Draft {
  return EMPTY_DRAFT;
}

export function buildPatch(draft: E5Draft, revision: string): { readonly patch: { readonly expected_revision: string; readonly set: readonly E5StagedChange[]; readonly unset: readonly string[] }; readonly controlIds: readonly string[] } {
  return {
    patch: {
      expected_revision: revision,
      set: [...draft.set.values()].sort((left, right) => left.key.localeCompare(right.key)),
      unset: [...draft.unset].sort(),
    },
    controlIds: [...draft.set.keys(), ...draft.unset].sort(),
  };
}

/** Commands that mutate daemon state and therefore require an ack. */
export const MUTATING_COMMANDS: ReadonlySet<E5CommandKind> = new Set<E5CommandKind>([
  'config.set',
  'provider.create',
  'provider.update',
  'provider.delete',
  'provider.rotate',
  'catalog.refresh',
  'catalog.probe',
  'model.select',
  'model.favorite',
  'generation.reset',
  'preset.save',
  'preset.duplicate',
  'role.save',
  'context.add',
  'context.remove',
  'context.retry',
  'context.pin',
  'context.exclude',
  'context.export',
  'context.reindex',
  'compaction.run',
  'compaction.cancel',
  'instruction.save',
  'instruction.toggle',
  'instruction.import',
  'memory.save',
  'memory.delete',
  'policy.apply',
  'policy.revoke',
  'approval.reply',
  'approval.remember',
  'mcp.create',
  'mcp.start',
  'mcp.stop',
  'mcp.restart',
  'mcp.test',
  'mcp.remove',
  'mcp.auth',
  'tool.enable',
  'tool.permissions',
  'tool.export',
  'skill.install',
  'skill.update',
  'skill.remove',
  'skill.enable',
  'skill.invoke',
  'command.insert',
  'hook.save',
  'hook.test',
  'terminal.background',
  'terminal.stop',
  'terminal.input',
  'terminal.reconcile',
  'browser.connect',
  'browser.capture',
  'budget.revise',
  'queue.edit',
  'queue.remove',
  'queue.next',
  'steer.send',
  'agent.pause',
  'agent.stop',
  'agent.model',
  'change.comment',
  'change.hunkAccept',
  'change.followup',
  'restore.run',
  'fork.run',
  'redo.run',
  'session.rename',
  'session.pin',
  'session.archive',
  'session.delete',
  'session.export',
  'session.fork',
  'usage.filter',
  'routing.save',
  'settings.import',
  'diagnostics.export',
  'diagnostics.lifecycle',
  'binding.revalidate',
  'binding.rebind',
  'finish.request',
]);

/** Presentation-only actions never claim a saved/verified state. */
export function actionAckText(command: E5CommandKind, result: string | null): string {
  if (result !== null && result.length > 0) return result;
  if (!MUTATING_COMMANDS.has(command)) return 'Served state re-read.';
  if (command === 'approval.reply') return 'The daemon accepted one exact waiter reply; the pending card drops only after the acknowledgement.';
  if (command === 'finish.request') return 'The finish request was acknowledged; the outcome stays Pending until every required fact matches.';
  if (command === 'restore.run') return 'The restore transaction was acknowledged; file restore is separate from a conversation fork.';
  if (command === 'policy.apply') return 'The policy version was acknowledged; future dispatch uses it.';
  return 'Acknowledged by the daemon.';
}
