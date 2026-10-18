// Pure Edition 8 view model: destinations, one status line, screen rows and
// per-control state. No VS Code or DOM imports, so selftest and the
// exercise/visual harnesses drive the same decisions as the panel.

import {
  E8_CONTROLS,
  E8_SCREENS,
  type E8Control,
  type E8Screen,
} from '../generated/edition8Registry.ts';
import {
  ageText,
  arrayAt,
  booleanAt,
  exactNumber,
  moneyMicro,
  numberAt,
  objectAt,
  stringAt,
  type E8ConfigValue,
  type E8Snapshot,
  type Json,
} from './contract.ts';
import {
  bindingFor8,
  optionsForBinding,
  type E8Binding,
} from './bindings.ts';
import { configValues } from './source.ts';

export type E8Destination = 'work' | 'inspect' | 'history' | 'settings';

export type E8ControlState = 'available' | 'empty' | 'loading' | 'stale' | 'error' | 'disabled';

export interface E8ControlView {
  readonly control: E8Control;
  readonly binding: E8Binding;
  readonly state: E8ControlState;
  readonly stateText: string;
  readonly valueText: string;
  readonly disabled: boolean;
  readonly disabledReason: string | null;
  readonly configValue: Json | null;
  readonly applyState: string | null;
  readonly source: string | null;
  readonly scope: string | null;
  readonly options: readonly string[];
  readonly routeText: string;
}

export interface E8Row {
  readonly key: string;
  readonly label: string;
  readonly value: string;
  readonly state: 'served' | 'unavailable' | 'warning' | 'error' | 'pending' | 'verified' | 'waiting';
}

export interface E8ScreenView {
  readonly screen: E8Screen;
  readonly destination: E8Destination;
  readonly rows: readonly E8Row[];
  readonly controls: readonly E8ControlView[];
  readonly stale: boolean;
  readonly staleText: string | null;
  readonly emptyText: string | null;
  readonly status: string;
  readonly statusLevel: 'quiet' | 'running' | 'waiting' | 'failed' | 'pending' | 'verified' | 'locked';
}

export interface E8PanelState {
  readonly snapshot: E8Snapshot | null;
  readonly loading: boolean;
  readonly error: string | null;
  readonly focusControl: string | null;
}

const SHELL_GROUPS = new Set(['Work', 'Inspect and proof', 'History']);
// Setup surfaces that live in the Work sidebar/modal flow rather than the
// editor-sized Settings surface (S33/S57 are settings tabs).
const WORK_SETUP = new Set(['S34', 'S35', 'S36', 'S37', 'S38', 'S39']);

export function destinationOf(screen: E8Screen): E8Destination {
  if (WORK_SETUP.has(screen.id)) return 'work';
  if (screen.group === 'Work') return 'work';
  if (screen.group === 'Inspect and proof') return 'inspect';
  if (screen.group === 'History') return 'history';
  return 'settings';
}

export function screensFor(destination: E8Destination): readonly E8Screen[] {
  return E8_SCREENS.filter((screen) => destinationOf(screen) === destination);
}

export function screenById(screenId: string): E8Screen {
  const screen = E8_SCREENS.find((entry) => entry.id === screenId);
  if (screen === undefined) throw new Error(`edition8: unknown screen ${screenId}`);
  return screen;
}

export function controlsForScreen(screenId: string): readonly E8Control[] {
  return screenById(screenId).controls
    .map((id) => E8_CONTROLS.find((control) => control.id === id))
    .filter((control): control is E8Control => control !== undefined);
}

export function isShellScreen(screenId: string): boolean {
  return WORK_SETUP.has(screenId) || (SHELL_GROUPS.has(screenById(screenId).group) && destinationOf(screenById(screenId)) !== 'settings');
}

// ------------------------------------------------------------- config value

function configValueFor(values: readonly E8ConfigValue[], key: string | null): E8ConfigValue | null {
  if (key === null) return null;
  const exact = values.find((value) => value.key === key);
  if (exact !== undefined) return exact;
  const bracket = key.lastIndexOf('].');
  if (bracket >= 0) {
    const suffix = key.slice(bracket + 2);
    const candidates = values.filter((value) => value.key.endsWith(`.${suffix}`));
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

// ----------------------------------------------------------- prerequisites

function sourceError(snapshot: E8Snapshot, key: string): string | null {
  const source = snapshot.sources[key];
  if (source === undefined || source.state === 'ready') return null;
  return source.error ?? `${source.method} ${source.path} is unavailable.`;
}

const GATED_CONTROLS = new Set(['G-01', 'G-02', 'G-04', 'G-05', 'G-06', 'G-07', 'G-08', 'G-09', 'G-10', 'G-11', 'G-12', 'G-13', 'G-14']);

function prerequisite(controlId: string, snapshot: E8Snapshot | null): string | null {
  if (snapshot === null) return null;
  if (GATED_CONTROLS.has(controlId) && text(snapshot.parameters, 'authority', 'capabilityAuthority') === 'unknown') {
    return 'The served capability authority is unknown; this gated control is disabled rather than guessed.';
  }
  const sessionRequired = bindingFor8(E8_CONTROLS.find((entry) => entry.id === controlId) as E8Control).route.path.includes(':session');
  if (sessionRequired && snapshot.binding?.session === null) {
    return 'No conversation is bound; open or create a conversation first.';
  }
  switch (controlId) {
    case 'M-01':
    case 'M-02':
    case 'M-07':
    case 'M-09':
      return snapshot.models === null ? `Missing prerequisite: GET /models. ${sourceError(snapshot, 'models') ?? ''}`.trim() : null;
    case 'M-11':
      return arrayAt(snapshot.receipts, 'receipts').length === 0
        ? 'No durable call receipt exists yet (no call was dispatched).'
        : null;
    case 'M-13':
    case 'O-06':
      return snapshot.usage === null ? `Missing prerequisite: GET /native/usage. ${sourceError(snapshot, 'usage') ?? ''}`.trim() : null;
    case 'P-10':
    case 'P-14':
      return arrayAt(snapshot.connections, 'connections').length === 0 ? 'No provider connection is served; add one first.' : null;
    case 'P-13':
    case 'O-04':
      return sourceError(snapshot, 'health') === null ? null : sourceError(snapshot, 'health');
    case 'C-01':
    case 'C-02':
    case 'C-12':
      return snapshot.binding?.session === null ? 'Context usage is session-scoped; no conversation is bound.' : null;
    case 'K-03':
      return snapshot.binding?.session === null ? 'Compaction is session-scoped; no conversation is bound.' : null;
    case 'K-11':
      return arrayAt(snapshot.memory, 'facts').length === 0 && arrayAt(snapshot.memory, 'records').length === 0
        ? 'No saved memory record is served; there is nothing to delete.'
        : null;
    case 'Q-06':
      return arrayAt(snapshot.approvals, 'permissions').length === 0 ? 'No live permission waiter exists.' : null;
    case 'T-06':
      return null;
    case 'T-09':
    case 'T-12':
      return arrayAt(snapshot.mcp, 'servers').length === 0 ? 'No MCP server is configured.' : null;
    case 'T-13':
      return arrayAt(snapshot.mcp, 'servers').length === 0 ? 'No MCP server is configured.' : null;
    case 'S-03':
    case 'S-04':
    case 'S-14':
      return arrayAt(snapshot.skills, 'skills').length === 0 ? 'No skill is installed; the library is not fabricated.' : null;
    case 'S-13':
      return arrayAt(snapshot.hooks, 'hooks').length === 0 ? 'No saved hook version exists to test.' : null;
    case 'X-01':
    case 'X-03':
    case 'X-05':
      return snapshot.binding?.session === null ? 'Terminal ownership is session-scoped; no conversation is bound.' : null;
    case 'X-06':
      return sourceError(snapshot, 'browser');
    case 'V-04':
    case 'V-05':
    case 'V-06':
    case 'V-12':
      return snapshot.binding?.session === null ? 'Checkpoints are session-scoped; no conversation is bound.' : null;
    case 'V-09':
      return snapshot.proof === null ? 'No task proof is served; the engineering outcome is Unavailable.' : null;
    case 'D-03':
    case 'D-05':
    case 'D-07':
    case 'D-13':
      return snapshot.binding?.session === null ? 'This read is session-scoped; no conversation is bound.' : null;
    case 'O-03':
      return sourceError(snapshot, 'diagnostics');
    case 'O-08':
      return sourceError(snapshot, 'enterprise') ?? (stringAt(snapshot.enterprise, 'state') === 'disabled' ? 'Enterprise features are disabled in this daemon configuration.' : null);
    case 'O-11':
      return sourceError(snapshot, 'workers') ?? (arrayAt(snapshot.workers, 'workers').length === 0 ? 'No remote worker is registered; local execution remains the default.' : null);
    default:
      return null;
  }
}

export function controlView(
  control: E8Control,
  snapshot: E8Snapshot | null,
  loading: boolean,
  error: string | null,
): E8ControlView {
  const binding = bindingFor8(control);
  const values = snapshot === null ? [] : configValues(snapshot.config);
  const config = configValueFor(values, binding.configKey);
  const options = optionsForBinding(binding, snapshot);
  let state: E8ControlState = 'available';
  let stateText = '';
  if (binding.kind === 'missing') {
    state = 'disabled';
    stateText = binding.route.reason ?? 'This capability is not served by the daemon.';
  } else if (snapshot === null) {
    state = loading ? 'loading' : 'empty';
    stateText = loading
      ? 'Loading the served state; the last coherent snapshot stays until it arrives.'
      : 'No Edition 8 state is served yet. Refresh names the missing read.';
  } else if (error !== null) {
    state = 'error';
    stateText = error;
  } else {
    const reason = prerequisite(control.id, snapshot);
    if (reason !== null) {
      state = 'disabled';
      stateText = reason;
    } else if (config?.locked === true) {
      state = 'disabled';
      stateText = `Locked by ${config.source}; the served effective value is read-only.`;
    } else if (snapshot.stale) {
      state = 'stale';
      stateText = `Stale by ${ageText(snapshot.stale_age_ms)}; admission and apply stay disabled until the binding is reconciled.`;
    }
  }
  const valueText =
    binding.kind === 'missing'
      ? 'Unavailable'
      : config !== null
        ? `${jsonText(config.value)} · ${config.source} · ${config.scope}`
        : binding.kind === 'host'
          ? 'Host-owned'
          : binding.input === 'readonly'
            ? `Read-only served value · ${binding.route.method} ${binding.route.path}`
            : 'Inherit (not configured)';
  return {
    control,
    binding,
    state,
    stateText,
    valueText,
    disabled: state === 'disabled' || state === 'error' || state === 'loading' || state === 'stale',
    disabledReason: state === 'disabled' || state === 'error' ? stateText : null,
    configValue: config === null ? null : config.value,
    applyState: config === null ? null : config.apply_state,
    source: config === null ? null : config.source,
    scope: config === null ? null : config.scope,
    options,
    routeText: `${binding.route.method} ${binding.route.path}`,
  };
}

// --------------------------------------------------------------- screen rows

function row(key: string, label: string, value: string | null, state: E8Row['state'] = 'served'): E8Row {
  return {
    key,
    label,
    value: value === null || value.length === 0 ? 'Unavailable' : value,
    state: value === null || value.length === 0 ? 'unavailable' : state,
  };
}

function list(value: Json | null, ...keys: string[]): readonly Json[] {
  for (const key of keys) {
    const found = arrayAt(value, key);
    if (found.length > 0) return found;
  }
  return Array.isArray(value) ? value : [];
}

function text(value: Json | null | undefined, ...keys: string[]): string | null {
  for (const key of keys) {
    const found = stringAt(value, key);
    if (found !== null) return found;
  }
  return null;
}

function healthText(snapshot: E8Snapshot): string | null {
  const ok = booleanAt(snapshot.health, 'ok');
  const version = text(snapshot.health, 'version');
  if (ok === null && version === null) return null;
  return `${ok === true ? 'Running' : ok === false ? 'Not healthy' : 'Observed'}${version === null ? '' : ` · ${version}`}`;
}

function sessionUsageText(snapshot: E8Snapshot): string | null {
  const usage = snapshot.sessionUsage;
  if (usage === null) return null;
  const tokens = numberAt(usage, 'tokens') ?? numberAt(usage, 'tokensUsed') ?? numberAt(usage, 'inputTokens');
  const cost = moneyMicro(objectAt(usage, 'costMicro') ?? objectAt(usage, 'spentMicro') ?? null);
  const parts: string[] = [];
  if (tokens !== null) parts.push(`${exactNumber(tokens)} tokens`);
  if (cost !== 'Unavailable') parts.push(cost);
  return parts.length === 0 ? null : parts.join(' · ');
}

function proofText(snapshot: E8Snapshot): string | null {
  const proof = snapshot.proof;
  if (proof === null) return null;
  const state = text(proof, 'proofState', 'state');
  const gate = text(objectAt(proof, 'gate'), 'status');
  if (state === null && gate === null) return null;
  return `${state ?? 'pending'}${gate === null ? '' : ` · gate ${gate}`}`;
}

function criteriaRows(snapshot: E8Snapshot, prefix: string, cap: (key: string, label: string, value: string | null, state?: E8Row['state']) => void): void {
  const criteria = list(objectAt(snapshot.proof, 'criteria'), 'criteria');
  for (const [index, entry] of criteria.slice(0, 20).entries()) {
    const id = text(entry, 'id', 'criterionId') ?? `AC-${String(index + 1).padStart(2, '0')}`;
    const verdict = text(entry, 'verdict', 'status') ?? 'pending';
    const title = text(entry, 'title', 'text', 'description') ?? '';
    cap(
      `${prefix}-criterion-${index}`,
      id,
      `${title}${title.length > 0 ? ' · ' : ''}${verdict}`,
      verdict === 'pass' ? 'verified' : verdict === 'fail' ? 'error' : 'pending',
    );
  }
}

function rowsForScreen(screenId: string, snapshot: E8Snapshot | null): readonly E8Row[] {
  if (snapshot === null) return [];
  const rows: E8Row[] = [];
  const cap = (key: string, label: string, value: string | null, state: E8Row['state'] = 'served') => {
    if (rows.length < 140) rows.push(row(key, label, value, state));
  };
  const values = configValues(snapshot.config);
  const config = (key: string): string | null => {
    const found = values.find((entry) => entry.key === key);
    return found === undefined ? null : `${jsonText(found.value)} · ${found.source}`;
  };
  const shell = (): string | null => config('faktor.sandbox.shell');
  const network = (): string | null => config('faktor.sandbox.network');
  const defaultModel = (): string | null => config('faktor.defaultModel');
  const defaultProvider = (): string | null => config('faktor.defaultProvider');

  switch (screenId) {
    case 'S34':
      cap('health', 'Service', healthText(snapshot), healthText(snapshot) === null ? 'unavailable' : 'served');
      cap('ready', 'Readiness', stringAt(snapshot.ready, 'ready') ?? (booleanAt(snapshot.ready, 'ready') === true ? 'Ready' : null));
      cap('binary', 'Service executable', config('faktor.runtime.binaryPath'));
      cap('model', 'Default model', defaultModel());
      cap('provider', 'Default provider', defaultProvider());
      cap('shell', 'Shell isolation', shell());
      cap('source-health', 'Health source', sourceError(snapshot, 'health') ?? 'GET /native/health served');
      return rows;
    case 'S35': {
      const trust = objectAt(snapshot.runtime, 'trust');
      cap('binary', 'Exact executable path', text(trust, 'executablePath', 'path') ?? text(snapshot.runtime, 'executablePath') ?? config('faktor.runtime.binaryPath'));
      cap('checksum', 'Source checksum', text(trust, 'observedChecksum', 'checksum', 'sha256') ?? 'Not recorded');
      cap('signature', 'Publisher/signature', text(trust, 'signatureState', 'publisherSignature', 'signature') ?? 'Not verified');
      cap('version', 'Version', text(snapshot.runtime, 'version') ?? text(snapshot.health, 'version'));
      cap('trust', 'Trust', text(trust, 'state', 'trust') ?? 'Not recorded');
      return rows;
    }
    case 'S57': {
      const isolation = objectAt(snapshot.runtime, 'shellIsolation');
      cap('shell', 'Shell isolation', text(isolation, 'mode') ?? shell());
      cap('network', 'Network', text(isolation, 'networkGuarantee') ?? network());
      cap('filesystem', 'Filesystem', text(isolation, 'filesystemGuarantee'));
      cap('loopback', 'Allow loopback', config('faktor.sandbox.allowLoopback'));
      cap('platform', 'Platform capability', text(objectAt(snapshot.health, 'capabilities'), 'platform') ?? text(snapshot.capabilities, 'platform'));
      cap('sandbox', 'Sandbox state', text(objectAt(snapshot.policy, 'sandbox_state'), 'state') ?? text(snapshot.policy, 'sandbox_state'));
      return rows;
    }
    case 'S36':
      cap('provider', 'Provider', defaultProvider());
      cap('model', 'Model', defaultModel());
      cap('preset', 'Preset', config('faktor.permissions.preset'));
      cap('effects', 'Allowed tools', config('faktor.permissions.allowedTools'));
      cap('finish', 'Finish level', text(objectAt(snapshot.taskRuns, 'completion'), 'level') ?? 'Local');
      cap('token-cap', 'Token cap', config('faktor.budget.maxTokens'));
      cap('cost-cap', 'Money cap', config('faktor.budget.maxCostMicro'));
      cap('usage', 'Session usage', sessionUsageText(snapshot));
      return rows;
    case 'S33':
    case 'S21':
      cap('tokens', 'Token limit', config('faktor.budget.maxTokens'));
      cap('cost', 'Spending limit', config('faktor.budget.maxCostMicro'));
      cap('warn', 'Warn at', config('faktor.budget.warnAtPercent'));
      cap('attempts', 'Total attempts', config('faktor.retry.maxAttempts'));
      cap('base', 'Base delay', config('faktor.retry.baseDelayMs'));
      cap('max', 'Max delay', config('faktor.retry.maxDelayMs'));
      cap('jitter', 'Jitter', config('faktor.retry.jitter'));
      cap('reservations', 'Reservations', snapshot.sessionUsage === null ? null : sessionUsageText(snapshot));
      cap('contract', 'Completion contract', text(objectAt(snapshot.taskRuns, 'completion'), 'contract') ?? text(objectAt(snapshot.proof, 'contract'), 'contract'));
      return rows;
    case 'S37':
      cap('sources', 'Draft sources', 'Host draft; failed/unsupported rows stay visible until removed or retried.');
      cap('queue', 'Admission', `Blocked while an invalid source remains (route ${snapshot.sources.receipts.path}).`, 'warning');
      return rows;
    case 'S38':
      cap('starting', 'Admission', text(objectAt(snapshot.taskRuns, 'run'), 'state') ?? 'No admission is pending.');
      cap('queue', 'Next message', 'Queued inputs are acknowledged only after admission.');
      cap('receipt', 'Receipt lookup', sourceError(snapshot, 'receipts') ?? 'GET /native/call-receipts served');
      return rows;
    case 'S39':
      cap('admission', 'Start confirmation', text(objectAt(snapshot.taskRuns, 'run'), 'state') ?? 'No ambiguous admission is served.');
      cap('receipt', 'Original request identity', snapshot.sources.receipts === undefined ? null : `${snapshot.sources.receipts.path} · ${snapshot.sources.receipts.state}`);
      cap('draft', 'Kept draft', 'Preserved in the host; retry uses the same request identity.');
      return rows;
    case 'S01':
      cap('run', 'Live run', text(objectAt(snapshot.taskRuns, 'run'), 'state') ?? text(objectAt(snapshot.usage, 'run'), 'state') ?? 'No active run is served.');
      cap('model', 'Model', defaultModel());
      cap('provider', 'Provider', defaultProvider());
      cap('effort', 'Reasoning effort', config('faktor.generation.reasoning') ?? text(snapshot.parameters, 'reasoning'));
      cap('preset', 'Authority', config('faktor.permissions.preset'));
      cap('context', 'Context usage', sessionUsageText(snapshot));
      cap('approval', 'Pending approval', String(arrayAt(snapshot.approvals, 'permissions').length));
      cap('queue', 'Queued next', String(arrayAt(snapshot.taskRuns, 'queued').length));
      cap('usage', 'Usage', snapshot.usage === null ? null : 'GET /native/usage served');
      cap('budget', 'Run budget', config('faktor.budget.maxTokens'));
      return rows;
    case 'S14':
      for (const [index, request] of arrayAt(snapshot.approvals, 'permissions').slice(0, 10).entries()) {
        cap(`approval-${index}`, text(request, 'capability', 'id') ?? `Request ${index + 1}`, `${text(request, 'detail') ?? 'No detail'} · ${text(request, 'session_id') ?? 'session unavailable'}`, 'waiting');
      }
      if (arrayAt(snapshot.approvals, 'permissions').length === 0) cap('approval-none', 'Approval', 'No permission is waiting.', 'unavailable');
      cap('policy', 'Effective policy', text(snapshot.policy, 'preset', 'version'));
      return rows;
    case 'S40':
      cap('question', 'Question', text(objectAt(snapshot.taskRuns, 'question'), 'text') ?? 'No task question is waiting.');
      cap('answer', 'Answer', 'Validated against the current task contract; no contract field is rewritten.');
      return rows;
    case 'S22':
      cap('activity', 'Activity', text(objectAt(snapshot.taskRuns, 'run'), 'state') ?? 'No live activity is served.');
      for (const [index, item] of list(snapshot.taskRuns, 'timeline', 'runs').slice(0, 12).entries()) {
        cap(`activity-${index}`, text(item, 'kind', 'state') ?? `Event ${index + 1}`, `${text(item, 'state', 'status') ?? 'pending'} · ${text(item, 'detail') ?? ''}`, 'pending');
      }
      cap('queue', 'Queued next', String(arrayAt(snapshot.taskRuns, 'queued').length));
      cap('approval', 'Approval', String(arrayAt(snapshot.approvals, 'permissions').length));
      return rows;
    case 'S59':
      for (const [index, item] of list(snapshot.taskRuns, 'queued', 'queue').slice(0, 20).entries()) {
        cap(`queue-${index}`, text(item, 'state') ?? `Queued ${index + 1}`, text(item, 'text') ?? null);
      }
      cap('queue-count', 'Queued inputs', String(arrayAt(snapshot.taskRuns, 'queued').length));
      return rows;
    case 'S70':
      cap('run-scope', 'Run', 'Cancels the run; already-issued commands and network calls are not undone.');
      cap('response-scope', 'Current response', 'Aborts the in-flight turn; the run and candidate remain.');
      cap('process-scope', 'Owned process', 'Stops only the named owned process; task stop and session abort stay separate.');
      return rows;
    case 'S41':
      cap('outcome', 'Engineering outcome', proofText(snapshot));
      cap('required', 'Required acceptance', text(objectAt(snapshot.proof, 'requiredChecks'), 'total', 'passed') ?? null);
      cap('claims', 'Claims', text(objectAt(snapshot.proof, 'review'), 'status'));
      cap('integration', 'Integration', text(objectAt(snapshot.proof, 'integration'), 'state'));
      cap('finish', 'Finish', text(objectAt(snapshot.proof, 'publication'), 'state'));
      return rows;
    case 'S42':
      for (const [index, step] of list(objectAt(snapshot.proof, 'completionSteps'), 'steps').slice(0, 24).entries()) {
        cap(`step-${index}`, text(step, 'step', 'id') ?? `Step ${index + 1}`, `${text(step, 'status') ?? 'pending'}${text(step, 'detail') === null ? '' : ` · ${text(step, 'detail')}`}`, text(step, 'status') === 'pass' ? 'verified' : 'pending');
      }
      cap('plan', 'Plan source', sourceError(snapshot, 'proof') ?? 'GET /native/tasks/{id}/proof served');
      return rows;
    case 'S24':
      cap('candidate', 'Candidate', text(objectAt(snapshot.verification, 'candidate'), 'id', 'digest') ?? text(snapshot.verification, 'candidate'));
      cap('criteria', 'Mapped criteria', text(objectAt(snapshot.verification, 'criteria'), 'total') ?? null);
      cap('diff', 'Native diff', 'Opened through vscode.diff; immutable base/candidate titles.');
      cap('pending', 'Pending verification', text(snapshot.verification, 'state', 'status'));
      return rows;
    case 'S43':
      cap('base', 'Base', text(objectAt(snapshot.verification, 'diff'), 'base') ?? 'Unavailable');
      cap('candidate', 'Candidate', text(objectAt(snapshot.verification, 'diff'), 'candidate') ?? 'Unavailable');
      cap('criteria', 'Related criterion', 'Opened from the inspected candidate; the diff is read-only.');
      return rows;
    case 'S45':
      criteriaRows(snapshot, 'required', cap);
      cap('gate', 'Gate', text(objectAt(snapshot.proof, 'gate'), 'status', 'reason'));
      return rows;
    case 'S46':
      cap('evidence', 'Evidence', text(objectAt(snapshot.proof, 'evidence'), 'id', 'digest') ?? 'No evidence detail is served.');
      cap('excerpt', 'Bounded excerpt', text(objectAt(snapshot.proof, 'evidence'), 'excerpt') ?? 'Open full output opens the bounded artifact.');
      return rows;
    case 'S47':
      cap('proof', 'Proof identity', text(snapshot.proof, 'verifiedDigest', 'candidateDigest'));
      cap('current', 'Current candidate', text(snapshot.verification, 'candidateDigest', 'digest'));
      cap('mismatch', 'Match', text(objectAt(snapshot.proof, 'landed'), 'landedEqualsVerified') ?? 'Unavailable');
      return rows;
    case 'S44':
      cap('conflict', 'Integration conflict', text(snapshot.verification, 'conflict', 'state') ?? 'No conflict is served.');
      cap('preconditions', 'Changed preconditions', text(objectAt(snapshot.verification, 'conflict'), 'path', 'precondition'));
      cap('edits', 'User edits', 'Preserved; restore/integration refuses changed preconditions.');
      return rows;
    case 'S48':
      cap('finish', 'Requested finish', config('faktor.completion.level') ?? text(objectAt(snapshot.proof, 'publication'), 'requested'));
      cap('receipt', 'Finish receipt', text(objectAt(snapshot.proof, 'publication'), 'receipt', 'commit'));
      cap('scm', 'SCM', text(objectAt(snapshot.proof, 'integration'), 'remote', 'repo') ?? 'Unavailable');
      cap('blocked', 'Blocker', text(objectAt(snapshot.proof, 'gate'), 'reason'));
      return rows;
    case 'S32':
      cap('outcome', 'Outcome', proofText(snapshot));
      cap('required', 'Required checks', text(objectAt(snapshot.proof, 'requiredChecks'), 'passed') ?? null);
      criteriaRows(snapshot, 'outcome', cap);
      cap('cost', 'Cost fold', text(objectAt(snapshot.proof, 'cost'), 'state') ?? null);
      cap('usage', 'Usage', sessionUsageText(snapshot));
      return rows;
    case 'S65':
      cap('verified', 'Verified candidate', text(objectAt(snapshot.proof, 'verified'), 'digest') ?? 'Unavailable');
      cap('landed', 'Landed', text(objectAt(snapshot.proof, 'landed'), 'digest') ?? 'Unavailable');
      cap('match', 'Landed equals verified', text(objectAt(snapshot.proof, 'landed'), 'landedEqualsVerified') ?? 'Unavailable');
      cap('finish', 'Finish', text(objectAt(snapshot.proof, 'publication'), 'state'));
      return rows;
    case 'S25':
      for (const [index, checkpoint] of list(snapshot.checkpoints, 'checkpoints').slice(0, 20).entries()) {
        cap(`checkpoint-${index}`, text(checkpoint, 'id', 'sequence') ?? `Checkpoint ${index + 1}`, `${text(checkpoint, 'coverage', 'state') ?? 'coverage unavailable'} · ${text(checkpoint, 'created_at', 'createdMs') ?? ''}`, 'served');
      }
      cap('coverage', 'Coverage', sourceError(snapshot, 'checkpoints') ?? 'GET /native/session/{id}/checkpoints served');
      return rows;
    case 'S49':
      cap('cancelled', 'Cancelled run', text(objectAt(snapshot.taskRuns, 'run'), 'state') ?? 'No cancelled run is served.');
      cap('kept', 'Kept work', text(objectAt(snapshot.taskRuns, 'run'), 'keptWork') ?? 'Checkpoints and candidate remain reachable.');
      cap('spend', 'Attempted spend', sessionUsageText(snapshot));
      return rows;
    case 'S23':
      for (const [index, agent] of list(snapshot.agents, 'agents').slice(0, 20).entries()) {
        cap(`agent-${index}`, text(agent, 'role', 'id') ?? `Agent ${index + 1}`, `${text(agent, 'state', 'status') ?? 'unknown'} · ${text(agent, 'model') ?? 'model unavailable'} · ${text(agent, 'parent_id') === null ? 'root' : `parent ${text(agent, 'parent_id')}`}`, text(agent, 'state') === 'running' ? 'served' : 'pending');
      }
      cap('budget', 'Shared budget', config('faktor.budget.maxTokens'));
      return rows;
    case 'S50':
      cap('agent', 'Agent', text(list(snapshot.agents, 'agents')[0], 'id', 'role'));
      cap('steer', 'Steer', 'Empty or >500 characters is rejected without truncation.');
      cap('ownership', 'Ownership', text(list(snapshot.agents, 'agents')[0], 'worktree', 'ownership'));
      return rows;
    case 'S51':
      for (const [index, tournament] of list(snapshot.tournaments, 'tournaments').slice(0, 10).entries()) {
        cap(`comparison-${index}`, text(tournament, 'id', 'state') ?? `Comparison ${index + 1}`, `${text(tournament, 'winner') ?? 'no winner yet'} · ${text(tournament, 'state') ?? ''}`);
      }
      cap('comparison', 'Comparisons', sourceError(snapshot, 'tournaments') ?? 'GET /native/session/{id}/tournaments served');
      return rows;
    case 'S52':
      for (const [index, post] of list(objectAt(snapshot.board, 'board'), 'posts').slice(0, 20).entries()) {
        cap(`note-${index}`, text(post, 'author', 'from') ?? `Note ${index + 1}`, text(post, 'text', 'body') ?? null);
      }
      cap('board', 'Coordination board', sourceError(snapshot, 'board') ?? 'GET /native/session/{id}/board served');
      return rows;
    case 'S26':
    case 'S61':
      for (const [index, session] of list(snapshot.sessions, 'sessions').slice(0, 30).entries()) {
        cap(`session-${index}`, text(session, 'title', 'id') ?? `Conversation ${index + 1}`, `${text(session, 'state') ?? 'unknown'} · ${text(session, 'model') ?? ''} · ${text(session, 'updatedMs', 'updated_ms') ?? ''}`, text(session, 'active') === 'true' ? 'served' : 'served');
      }
      if (list(snapshot.sessions, 'sessions').length === 0) cap('empty', 'Conversations', 'No conversations yet. Start a Task.', 'unavailable');
      return rows;
    case 'S71':
      for (const [index, hit] of list(snapshot.search, 'results', 'hits').slice(0, 30).entries()) {
        cap(`hit-${index}`, text(hit, 'role', 'id') ?? `Result ${index + 1}`, text(hit, 'snippet', 'text') ?? null);
      }
      cap('search', 'Transcript search', sourceError(snapshot, 'search') ?? 'GET /native/transcript/search served');
      return rows;
    case 'S72':
      cap('target', 'Target conversation', snapshot.binding?.session ?? 'No conversation bound');
      cap('draft', 'Draft/attachments/caret/scroll', 'Saved per conversation; switching does not cancel the other run.');
      cap('binding', 'Generation', snapshot.binding?.generation === null || snapshot.binding?.generation === undefined ? null : String(snapshot.binding.generation));
      return rows;
    case 'S53':
    case 'S27':
    case 'S54':
      cap('spend', 'Attempted spend', moneyMicro(objectAt(snapshot.usage, 'attemptedSpendMicro') ?? objectAt(snapshot.usage, 'attempted_micro') ?? null));
      cap('verified', 'Verified landed changes', (() => { const value = numberAt(snapshot.outcomes, 'verifiedLandedChanges') ?? numberAt(snapshot.outcomes, 'verified_landed'); return value === null ? null : exactNumber(value); })());
      cap('vcsc', 'Cost per verified change', text(snapshot.outcomes, 'vcsc', 'vcscMicro') ?? text(snapshot.usage, 'vcsc'));
      cap('failed', 'Failed/cancelled attempts', (() => { const value = numberAt(snapshot.outcomes, 'failedAttempts') ?? numberAt(snapshot.outcomes, 'failed'); return value === null ? null : exactNumber(value); })());
      cap('local', 'Local routes', text(snapshot.usage, 'local'));
      cap('hosted', 'Hosted routes', text(snapshot.usage, 'hosted'));
      cap('quota', 'Quota', text(snapshot.entitlements, 'quota', 'status') ?? 'Unavailable');
      cap('account', 'Account', snapshot.identity === null ? 'No account required for local work.' : text(snapshot.identity, 'organization', 'user'));
      cap(
        'efficiency',
        'Efficiency effects',
        (() => {
          const compactions = list(snapshot.efficiency, 'compaction');
          const cache = text(objectAt(snapshot.efficiency, 'cache'), 'state');
          if (compactions.length === 0 && cache === null) return sourceError(snapshot, 'efficiency') ?? 'No measured effect is served.';
          const last = compactions[0];
          return `${compactions.length} compaction effect(s)${last === undefined ? '' : ` · last reduction ${exactNumber(numberAt(last, 'reduction') ?? 0)}`} · cache ${cache ?? 'unavailable'}`;
        })(),
      );
      cap('budget', 'Task budget', text(snapshot.budget, 'state') ?? 'Unavailable');
      return rows;
    case 'S02':
    case 'S63':
      for (const [index, model] of list(snapshot.models, '').slice(0, 40).entries()) {
        const provider = text(model, 'provider') ?? '';
        const id = text(model, 'model', 'id') ?? '';
        const available = text(model, 'availability') ?? text(model, 'state') ?? 'unknown';
        cap(`model-${index}`, `${provider}/${id}`, `${available} · authority ${text(model, 'capabilityAuthority', 'authority') ?? 'unknown'}`, available === 'available' ? 'served' : 'unavailable');
      }
      cap('parameters', 'Capability sheet', sourceError(snapshot, 'parameters') ?? 'GET /native/model-parameters served');
      cap('comparison', 'Comparison', text(snapshot.tournaments, 'state') ?? 'No comparison is active.');
      return rows;
    case 'S03':
      for (const [index, connection] of list(snapshot.connections, 'connections').slice(0, 20).entries()) {
        cap(`connection-${index}`, text(connection, 'label', 'id') ?? `Connection ${index + 1}`, `${text(connection, 'family') ?? ''} · ${text(connection, 'endpoint') ?? ''} · credential ${text(connection, 'credential_ref', 'credentialRef') ?? 'unbound'} · ${text(connection, 'apply_state', 'applyState') ?? ''}`);
      }
      cap('connections', 'Connections', arrayAt(snapshot.connections, 'connections').length === 0 ? 'No connections are served; add one to begin.' : 'GET /native/providers/connections served');
      return rows;
    case 'S04':
      cap('local', 'Local runtime', text(snapshot.providers, 'local') ?? text(list(snapshot.providers, 'providers')[0], 'endpoint'));
      cap('daemon', 'Daemon host', snapshot.binding?.daemon ?? null);
      cap('loopback', 'Loopback', config('faktor.sandbox.allowLoopback'));
      cap('lan', 'Private LAN', 'Refused under current provider policy; allow_loopback is not a bypass.');
      cap('embedding', 'Embedding', config('faktor.retrieval.embeddingProvider'));
      return rows;
    case 'S05':
      cap('model', 'Model', `${defaultProvider() ?? 'Unavailable'}/${defaultModel() ?? 'Unavailable'}`);
      cap('timing', 'Apply timing', text(snapshot.parameters, 'applyTiming', 'apply_timing'));
      for (const [index, parameter] of list(objectAt(snapshot.parameters, 'parameters'), 'parameters').slice(0, 30).entries()) {
        cap(`parameter-${index}`, text(parameter, 'key', 'name') ?? `Parameter ${index + 1}`, `${text(parameter, 'kind') ?? ''} ${text(parameter, 'min') === null ? '' : `· min ${text(parameter, 'min')}`}${text(parameter, 'max') === null ? '' : ` · max ${text(parameter, 'max')}`}`);
      }
      cap('authority', 'Authority', text(snapshot.parameters, 'authority', 'capability_authority'));
      cap('effective', 'Effective request', text(snapshot.parameters, 'effective') ?? 'No request captured yet.');
      return rows;
    case 'S06':
      for (const [index, preset] of list(snapshot.presets, 'presets').slice(0, 20).entries()) {
        cap(`preset-${index}`, text(preset, 'name', 'id') ?? `Preset ${index + 1}`, `${text(preset, 'mode') ?? 'unknown'} · ${text(preset, 'modelProvider') ?? ''}/${text(preset, 'modelId') ?? ''} · v${text(preset, 'version') ?? '?'}${text(preset, 'locked') === 'true' ? ' · locked' : ''}`);
      }
      for (const [index, agent] of list(snapshot.agents, 'agents').slice(0, 10).entries()) {
        cap(`agent-${index}`, text(agent, 'role', 'id') ?? `Agent ${index + 1}`, `${text(agent, 'state') ?? 'unknown'} · ${text(agent, 'model') ?? ''}`);
      }
      cap('presets', 'Presets', sourceError(snapshot, 'presets') ?? 'GET /native/presets served');
      return rows;
    case 'S07':
      cap('presets', 'Presets', sourceError(snapshot, 'presets') ?? 'GET /native/presets served');
      cap('envelope', 'Envelope', text(list(snapshot.presets, 'presets')[0], 'id') === null ? 'Save Agent resolves the envelope through /native/presets/{id}/envelope.' : 'POST /native/presets saves; envelope is read back from /native/presets/{id}/envelope.');
      cap('role-model', 'Role model', 'PresetDraft stores one modelProvider/modelId; no four-role map DTO is served.');
      return rows;
    case 'S08':
    case 'S62':
      cap('usage', 'Context usage', sessionUsageText(snapshot) ?? 'Unavailable');
      cap('basis', 'Measurement basis', text(objectAt(snapshot.sessionUsage, 'context'), 'basis') ?? 'Unavailable');
      cap('capacity', 'Capacity', text(objectAt(snapshot.sessionUsage, 'context'), 'capacity') ?? text(objectAt(snapshot.parameters, 'context'), 'limit'));
      cap('manifest', 'Context manifest', text(objectAt(snapshot.sessionUsage, 'context'), 'manifest', 'revision') ?? sourceError(snapshot, 'sessionUsage') ?? 'Unavailable');
      cap('reserve', 'Safety reserve', config('faktor.context.safetyReserve'));
      cap('index', 'Index coverage', text(snapshot.index, 'coverage') ?? sourceError(snapshot, 'index'));
      for (const [index, pin] of list(snapshot.contextPins, 'pins').slice(0, 10).entries()) {
        cap(`pin-${index}`, text(pin, 'sourceId', 'id') ?? `Pin ${index + 1}`, text(pin, 'action') ?? '');
      }
      return rows;
    case 'S09':
      cap('sources', 'Add context', 'Exact URI/range/version preserved across worktree/session switches.');
      cap('unsupported', 'Unsupported image', 'Visible with a typed reason; Retry/Remove remain available.');
      return rows;
    case 'S10':
      cap('threshold', 'Minimum reduction', config('faktor.context.compactionMinReduction'));
      cap('plan', 'Compaction plan', 'A durable compact request is required before any reduction is claimed.');
      cap('retained', 'Always kept', 'Task, criteria, constraints, candidate, evidence, permissions and approvals.');
      cap('result', 'Measured reduction', (() => { const compaction = objectAt(snapshot.sessionUsage, 'compaction'); const reduction = numberAt(compaction, 'reduction'); return reduction === null ? 'Unavailable until a durable compact result is read.' : exactNumber(reduction); })());
      return rows;
    case 'S11':
      for (const [index, record] of list(snapshot.instructions, 'records').slice(0, 30).entries()) {
        cap(`instruction-${index}`, text(record, 'title', 'id') ?? `Instruction ${index + 1}`, `${text(record, 'kind') ?? ''} · ${text(record, 'origin') ?? ''} · ${text(record, 'scope') ?? ''} · ${text(record, 'activation') ?? ''} · ${text(record, 'enabled') ?? 'unknown'}${text(record, 'locked') === 'true' ? ' · locked' : ''}`);
      }
      cap('instructions', 'Instruction sources', sourceError(snapshot, 'instructions') ?? 'GET /native/instructions/records served');
      cap('precedence', 'Precedence', 'Managed > project > user > imported; project text is not executable authority.');
      return rows;
    case 'S69':
      for (const [index, record] of list(snapshot.instructions, 'records').slice(0, 20).entries()) {
        cap(`precedence-${index}`, text(record, 'title', 'id') ?? `Rule ${index + 1}`, `${text(record, 'origin') ?? ''} · ${text(record, 'kind') ?? ''} · ${text(record, 'enabled') ?? 'unknown'}`);
      }
      cap('instructions', 'Instructions', sourceError(snapshot, 'instructions') ?? 'GET /native/instructions/records served');
      cap('conflicts', 'Conflicts', 'Conflicts are shown from the merged preview, never silently resolved.');
      return rows;
    case 'S12':
    case 'S66':
      for (const [index, fact] of list(snapshot.memory, 'records', 'facts').slice(0, 20).entries()) {
        cap(`memory-${index}`, text(fact, 'title', 'key', 'id') ?? `Memory ${index + 1}`, `${text(fact, 'scope') ?? ''} · ${text(fact, 'source', 'origin') ?? ''} · v${text(fact, 'version') ?? '?'} · ${text(fact, 'text', 'value', 'content') ?? ''}${text(fact, 'deleted') === 'true' ? ' · tombstoned' : ''}`);
      }
      cap('retention', 'Retention', config('faktor.memory.retentionDays'));
      cap('deletion', 'Deletion scope', 'Stops future retrieval; earlier prompts/artifacts/log copies are disclosed as retained.');
      return rows;
    case 'S13':
      cap('preset', 'Permission preset', config('faktor.permissions.preset'));
      cap('allowed', 'Allowed tools', config('faktor.permissions.allowedTools'));
      cap('shell', 'Shell', config('faktor.sandbox.shell'));
      cap('network', 'Network', config('faktor.sandbox.network'));
      cap('rules', 'Persistent rules', text(snapshot.policy, 'revision', 'version'));
      cap('managed', 'Organization lock', snapshot.enterprise === null ? 'No managed policy is served.' : text(objectAt(snapshot.enterprise, 'effectiveConfig'), 'locked'));
      return rows;
    case 'S15':
    case 'S16':
    case 'S68':
      for (const [index, server] of list(snapshot.mcp, 'servers').slice(0, 20).entries()) {
        const record = objectAt(server, 'record');
        cap(`mcp-${index}`, text(record, 'name', 'id') ?? text(server, 'id') ?? `Server ${index + 1}`, `${text(objectAt(server, 'health'), 'state') ?? 'stopped'} · ${text(objectAt(server, 'remoteTransport'), 'supported') === 'true' ? 'remote' : 'stdio'} · ${stringAt(objectAt(server, 'logs'), 'available') ?? 'logs unavailable'}`);
      }
      cap('bounds', 'Bounds', '8 servers, 128-byte name, 512-byte args, 32 args; failures precede persistence.');
      return rows;
    case 'S17':
    case 'S18':
    case 'S67':
      for (const [index, skill] of list(snapshot.skills, 'skills').slice(0, 20).entries()) {
        cap(`skill-${index}`, text(skill, 'id', 'name') ?? `Skill ${index + 1}`, `${text(skill, 'scope') ?? ''} · ${text(skill, 'invocation') ?? ''}`);
      }
      for (const [index, hook] of list(snapshot.hooks, 'hooks').slice(0, 20).entries()) {
        const record = objectAt(hook, 'record');
        cap(`hook-${index}`, text(record, 'event', 'id') ?? `Hook ${index + 1}`, `${text(record, 'command') ?? ''} · registered ${text(hook, 'registered') ?? 'unknown'}`);
      }
      cap('test', 'Hook test', text(list(snapshot.hookAudit, 'hooks', 'facts')[0], 'state', 'verdict') ?? 'No bounded test result is served.');
      return rows;
    case 'S19':
      for (const [index, process] of list(snapshot.terminals, 'terminals', 'processes').slice(0, 20).entries()) {
        cap(`process-${index}`, text(process, 'id', 'name') ?? `Process ${index + 1}`, `${text(process, 'state', 'status') ?? 'unknown'} · ${text(process, 'cwd') ?? ''}`);
      }
      cap('output', 'Output', 'Bounded 64 KiB visible tail; Open Full Output links the artifact.');
      cap('owner', 'Owner', text(list(snapshot.terminals, 'terminals', 'processes')[0], 'owner', 'session_id'));
      return rows;
    case 'S20':
      for (const [index, adapter] of list(snapshot.browserAdapters, 'adapters').slice(0, 10).entries()) {
        cap(`adapter-${index}`, text(adapter, 'id', 'name') ?? `Adapter ${index + 1}`, `${text(adapter, 'mode') ?? ''} · ${text(adapter, 'networkScope') ?? ''} · advertised ${text(adapter, 'advertised') ?? 'unknown'}`);
      }
      for (const [index, session] of list(snapshot.browserSessions, 'sessions').slice(0, 10).entries()) {
        cap(`browser-session-${index}`, text(session, 'id') ?? `Browser ${index + 1}`, `${text(session, 'state') ?? 'unknown'} · ${text(session, 'url') ?? ''}`);
      }
      cap('adapter', 'Adapter', text(snapshot.browserAdapters, 'state') ?? sourceError(snapshot, 'browserAdapters') ?? 'Unavailable');
      cap('url', 'URL', text(list(snapshot.browserSessions, 'sessions')[0], 'url') ?? 'No URL validated yet.');
      cap('console', 'Console/network', 'Observations are read from the adapter session; a preview alone is never treated as an observation.');
      return rows;
    case 'S28':
      cap('failure', 'Failure learning', config('faktor.efficiency.failureLearning'));
      cap('ccr', 'Compressed tool evidence', config('faktor.efficiency.ccr'));
      cap('handoff', 'Typed handoff', config('faktor.efficiency.typedHandoff'));
      cap('semantic', 'Information-gain context', config('faktor.efficiency.semanticContext'));
      cap('rework', 'Rework guard', config('faktor.efficiency.reworkGuard'));
      cap('retrieval', 'Retrieval', config('faktor.retrieval.strategy'));
      return rows;
    case 'S29':
    case 'S64':
      for (const value of values.slice(0, 60)) {
        cap(`config-${value.key}`, value.key, `${jsonText(value.value)} · ${value.source}${value.locked ? ' · locked' : ''}`);
      }
      for (const [index, rowValue] of list(snapshot.provenance, 'rows').slice(0, 40).entries()) {
        cap(`provenance-${index}`, text(rowValue, 'key') ?? `Setting ${index + 1}`, `${text(objectAt(rowValue, 'effective'), 'source') ?? 'unset'} · ${text(rowValue, 'applyTiming') ?? ''}`);
      }
      cap('count', 'Effective settings', String(values.length));
      return rows;
    case 'S60':
      cap('revision', 'Current revision', stringAt(snapshot.config, 'revision'));
      cap('runtime-revision', 'Runtime revision', stringAt(snapshot.provenance, 'runtimeRevision') ?? stringAt(snapshot.config, 'runtimeRevision'));
      for (const [index, conflict] of arrayAt(snapshot.provenance, 'conflicts').slice(0, 20).entries()) {
        cap(`conflict-${index}`, text(conflict, 'key') ?? `Conflict ${index + 1}`, text(conflict, 'reason') ?? 'Conflicting scopes are shown; no silent winner is claimed.');
      }
      cap('conflict', 'Conflict', 'A patch against a stale revision writes nothing and keeps the draft.');
      return rows;
    case 'S55':
      cap('identity', 'Account', snapshot.identity === null ? 'No optional account is connected.' : text(snapshot.identity, 'organization', 'user'));
      cap('endpoint', 'Control-plane endpoint', config('faktor.controlPlaneEndpoint'));
      cap('status', 'Connection status', text(snapshot.identity, 'status', 'state') ?? 'Local-only operation');
      return rows;
    case 'S30':
      cap('health', 'Service/version', healthText(snapshot));
      cap('ready', 'Readiness', text(snapshot.ready, 'ready') ?? sourceError(snapshot, 'ready'));
      cap('checksum', 'Checksum', text(objectAt(snapshot.runtime, 'trust'), 'observedChecksum', 'checksum', 'sha256') ?? text(snapshot.diagnostics, 'checksum') ?? 'Not recorded');
      cap('signature', 'Signature', text(objectAt(snapshot.runtime, 'trust'), 'signatureState', 'signature', 'publisherSignature') ?? text(snapshot.diagnostics, 'signature') ?? 'Not verified');
      cap('protocol', 'Protocol', text(objectAt(snapshot.diagnostics, 'protocol'), 'status') ?? text(snapshot.diagnostics, 'protocol_compatible'));
      cap('export', 'Export preview', sourceError(snapshot, 'diagnostics') ?? 'GET /native/diagnostics/preview served');
      cap('logs', 'Logs', 'Host log channel; redacted export excludes credentials and raw environment.');
      cap('updater', 'Updater', text(snapshot.updater, 'state', 'status') ?? 'Updater is disabled or unavailable.');
      return rows;
    case 'S56':
      cap('stale', 'Live updates', snapshot.stale ? `Paused · last coherent ${ageText(snapshot.stale_age_ms)} ago` : 'Live');
      cap('snapshot', 'Last snapshot', snapshot.generated_at_ms === 0 ? 'None' : new Date(snapshot.generated_at_ms).toISOString());
      cap('protocol', 'Protocol versions', text(objectAt(snapshot.diagnostics, 'protocol'), 'expected', 'received') ?? 'Unavailable');
      cap('draft', 'Draft', 'Preserved; nothing was skipped.');
      return rows;
    case 'S31':
      cap('editor', 'Editor', snapshot.binding?.workspace ?? null);
      cap('execution', 'Execution', snapshot.binding?.worktree ?? null);
      cap('daemon', 'Daemon', snapshot.binding?.daemon ?? null);
      cap('runtime', 'Runtime', text(snapshot.runtime, 'executablePath', 'version') ?? sourceError(snapshot, 'runtime'));
      cap('model', 'Model endpoint', defaultProvider());
      cap('vault', 'Credential vault', 'Machine-scoped secret store; never copied into the project.');
      cap('candidate', 'Candidate', text(objectAt(snapshot.verification, 'candidate'), 'worktree', 'digest'));
      return rows;
    case 'S58':
      cap('width', 'Panel', '320 px reference: one selected detail, stacked configuration, composer reachable.');
      cap('focus', 'Focus', text(snapshot.diagnostics, 'focus') ?? 'Host focus; never trapped behind sticky content.');
      cap('motion', 'Reduced motion', 'Static state cues remain when motion is reduced.');
      return rows;
    default:
      cap('summary', 'Served state', `${screenId} reads ${sourceError(snapshot, 'health') ?? 'GET /native/health'}`);
      return rows;
  }
}

function screenStatus(screen: E8Screen, snapshot: E8Snapshot | null, controls: readonly E8ControlView[]): { text: string; level: E8ScreenView['statusLevel'] } {
  if (snapshot === null) return { text: 'Not loaded', level: 'quiet' };
  if (snapshot.stale) return { text: `Stale · ${ageText(snapshot.stale_age_ms)}`, level: 'waiting' };
  if (controls.some((control) => control.state === 'error')) return { text: 'Action needed', level: 'failed' };
  if (controls.some((control) => control.state === 'disabled')) return { text: 'Some capabilities unavailable', level: 'quiet' };
  if (screen.id === 'S32' || screen.id === 'S65') {
    const proof = proofText(snapshot);
    if (proof !== null) return { text: proof, level: proof.startsWith('verified') ? 'verified' : 'pending' };
  }
  if (screen.id === 'S01' || screen.id === 'S22' || screen.id === 'S38' || screen.id === 'S59' || screen.id === 'S70') {
    const runState = text(objectAt(snapshot.taskRuns, 'run'), 'state', 'status');
    if (runState === null) return { text: arrayAt(snapshot.approvals, 'permissions').length > 0 ? 'Waiting for approval' : 'No active run', level: arrayAt(snapshot.approvals, 'permissions').length > 0 ? 'waiting' : 'quiet' };
    if (runState === 'running' || runState === 'active') return { text: 'Running', level: 'running' };
    if (runState === 'queued' || runState === 'starting' || runState === 'pending') return { text: 'Queued', level: 'pending' };
    if (runState === 'failed' || runState === 'error') return { text: 'Failed', level: 'failed' };
    if (runState === 'cancelled') return { text: 'Cancelled', level: 'waiting' };
    if (runState === 'verified') return { text: 'Verified', level: 'verified' };
    if (runState === 'complete') return { text: 'Complete (outcome pending)', level: 'pending' };
    return { text: runState, level: 'quiet' };
  }
  return { text: 'Ready', level: 'quiet' };
}

export function screenView(screenId: string, state: E8PanelState): E8ScreenView {
  const screen = screenById(screenId);
  const controls = controlsForScreen(screenId).map((control) =>
    controlView(control, state.snapshot, state.loading, state.error),
  );
  const staleText =
    state.snapshot !== null && state.snapshot.stale
      ? `Stale by ${ageText(state.snapshot.stale_age_ms)}; the last coherent snapshot is shown and admission/apply are disabled.`
      : null;
  const emptyText =
    state.snapshot === null && !state.loading && state.error === null
      ? 'No served state yet. Refresh names the missing read.'
      : null;
  const status = screenStatus(screen, state.snapshot, controls);
  return {
    screen,
    destination: destinationOf(screen),
    rows: rowsForScreen(screenId, state.snapshot),
    controls,
    stale: state.snapshot?.stale === true,
    staleText,
    emptyText,
    status: status.text,
    statusLevel: status.level,
  };
}

export function statusGlyph(level: E8ScreenView['statusLevel']): string {
  switch (level) {
    case 'running':
      return '\u25B6';
    case 'waiting':
      return '\u25CB';
    case 'failed':
      return '\u2716';
    case 'pending':
      return '\u25D4';
    case 'verified':
      return '\u2714';
    case 'locked':
      return '\u25A0';
    default:
      return '\u25CF';
  }
}
