// Every Revision 8 control maps to exactly one typed host command and one
// exact daemon route (or a typed missing/host-action reason). The controls'
// default, scope, apply timing and state copy come from the frozen registry;
// this file adds the executable binding only.

import { E8_CONTROLS, type E8Control } from '../generated/edition8Registry.ts';
import { bindingFor, type E5InputKind, type E5OptionsSource } from '../edition5/bindings.ts';
import { arrayAt, isObject, type E8Snapshot, type Json } from './contract.ts';
import { rawRoute, resolveRoute, type E8Route, type E8RouteKind } from './routes.ts';

export type E8InputKind = E5InputKind;

export interface E8Binding {
  readonly controlId: string;
  readonly route: E8Route;
  readonly input: E8InputKind;
  readonly configKey: string | null;
  readonly options: readonly string[];
  readonly optionsSource: E5OptionsSource;
  readonly actionLabel: string;
  readonly placeholder: string | null;
  readonly min: number | null;
  readonly max: number | null;
  readonly step: number | null;
  readonly unit: string | null;
  readonly sample: string | number | boolean;
  readonly kind: E8RouteKind;
}

const ROUTE_LABELS: Readonly<Record<string, string>> = {
  'M-01': 'Apply provider',
  'M-02': 'Apply model',
  'M-03': 'Toggle favorite',
  'M-04': 'Inspect routing',
  'M-05': 'Inspect role model',
  'M-06': 'Inspect fallback',
  'M-07': 'Inspect capabilities',
  'M-08': 'Probe model',
  'M-09': 'Apply context limit',
  'M-10': 'Inspect quality floors',
  'M-11': 'Inspect call receipt',
  'M-12': 'Inspect apply timing',
  'M-13': 'Inspect cost/limits',
  'M-14': 'Start comparison',
  'P-10': 'Test connection',
  'P-13': 'Inspect binding',
  'P-14': 'Rotate credential',
  'G-14': 'Inspect effective request',
  'A-01': 'Apply preset',
  'A-10': 'Open child controls',
  'C-03': 'Add context source',
  'C-05': 'Add attachment',
  'C-06': 'Admit paste',
  'C-07': 'Pin selected source',
  'C-11': 'Reindex',
  'C-12': 'Preview capacity',
  'C-14': 'Export manifest',
  'K-03': 'Preview compaction',
  'K-11': 'Review and delete',
  'I-08': 'Import rules',
  'Q-06': 'Reply to approval',
  'Q-08': 'Test against policy',
  'Q-09': 'Revoke rule',
  'Q-11': 'Open native setup',
  'Q-13': 'Open decision log',
  'T-02': 'Toggle tool',
  'T-06': 'Connect OAuth',
  'T-13': 'Restart server',
  'T-15': 'Export diagnostics',
  'S-02': 'Set invocation policy',
  'S-04': 'Install or update',
  'S-05': 'Insert command',
  'S-13': 'Run bounded test',
  'X-04': 'Run in background',
  'X-05': 'Stop owned process',
  'X-06': 'Connect browser',
  'X-08': 'Capture artifact',
  'X-10': 'Add diagnostics',
  'X-11': 'Add code action',
  'B-05': 'Request allowance revision',
  'B-08': 'Cancel retry',
  'B-11': 'Retry connection',
  'B-12': 'Look up receipt',
  'V-01': 'Open native diff',
  'V-02': 'Add review comment',
  'V-03': 'Accept selected hunks',
  'V-05': 'Preview restore',
  'V-06': 'Fork conversation',
  'D-02': 'Search titles',
  'D-03': 'Find in transcript',
  'D-04': 'Rename conversation',
  'D-05': 'Edit queue',
  'D-06': 'Export conversation',
  'D-07': 'Inspect timeline',
  'D-11': 'Open command help',
  'D-13': 'Open transcript window',
  'D-14': 'Open accessibility audit',
  'D-15': 'Switch worktree tab',
  'O-01': 'Search settings',
  'O-03': 'Export diagnostics',
  'O-04': 'Inspect runtime',
  'O-05': 'Start runtime',
  'O-07': 'Sign in',
  'O-08': 'Inspect managed locks',
  'O-12': 'Import configuration',
  'O-13': 'Inspect capability handshake',
  'S-12': 'Save hook',
  'D-08': 'Choose display mode',
  'D-09': 'Choose density',
  'D-10': 'Choose send behavior',
  'D-12': 'Choose notification classes',
  'O-06': 'Inspect usage',
  'O-09': 'Open retention',
  'O-10': 'Toggle telemetry',
  'O-11': 'Inspect workers',
};

function labelFor(control: E8Control, kind: E8RouteKind): string {
  const explicit = ROUTE_LABELS[control.id];
  if (explicit !== undefined) return explicit;
  if (kind === 'missing') return 'Unavailable';
  if (kind === 'host') return control.name;
  if (kind === 'config') return 'Stage value';
  return 'Inspect served state';
}

function optionsFor(control: E8Control, optionsSource: E5OptionsSource, snapshot: E8Snapshot | null): readonly string[] {
  const e5 = bindingFor(control as never);
  if (e5.options.length > 0) return e5.options;
  if (snapshot === null || optionsSource === null) return [];
  switch (optionsSource) {
    case 'models':
      return arrayAt(snapshot.models, '')
        .map((entry) => {
          if (!isObject(entry)) return null;
          const provider = entry.provider;
          const model = entry.model ?? entry.id;
          return typeof provider === 'string' && typeof model === 'string' ? `${provider}/${model}` : null;
        })
        .filter((entry): entry is string => entry !== null);
    case 'providers':
      return arrayAt(snapshot.connections, 'connections')
        .map((entry) => (isObject(entry) && typeof entry.id === 'string' ? entry.id : null))
        .filter((entry): entry is string => entry !== null);
    case 'mcp':
      return arrayAt(snapshot.mcp, 'servers')
        .map((entry) => {
          if (!isObject(entry)) return null;
          if (typeof entry.id === 'string') return entry.id;
          const record = entry.record;
          return isObject(record) && typeof record.id === 'string' ? record.id : null;
        })
        .filter((entry): entry is string => entry !== null);
    case 'skills':
      return arrayAt(snapshot.skills, 'skills')
        .map((entry) => (isObject(entry) && typeof entry.id === 'string' ? entry.id : null))
        .filter((entry): entry is string => entry !== null);
    case 'hooks':
      return arrayAt(snapshot.hooks, 'hooks')
        .map((entry) => {
          if (!isObject(entry)) return null;
          if (typeof entry.id === 'string') return entry.id;
          const record = entry.record;
          return isObject(record) && typeof record.id === 'string' ? record.id : null;
        })
        .filter((entry): entry is string => entry !== null);
    case 'tools':
      return arrayAt(snapshot.tools, 'builtin')
        .map((entry) => (isObject(entry) && typeof entry.id === 'string' ? entry.id : null))
        .filter((entry): entry is string => entry !== null);
    case 'processes':
      return arrayAt(snapshot.terminals, 'terminals')
        .map((entry) => (isObject(entry) && typeof entry.id === 'string' ? entry.id : null))
        .filter((entry): entry is string => entry !== null);
    case 'checkpoints':
      return arrayAt(snapshot.checkpoints, 'checkpoints')
        .map((entry) => (isObject(entry) && typeof entry.id === 'string' ? entry.id : null))
        .filter((entry): entry is string => entry !== null);
    case 'presets':
    case 'roles':
      return arrayAt(snapshot.agents, 'agents')
        .map((entry) => (isObject(entry) && typeof entry.id === 'string' ? entry.id : null))
        .filter((entry): entry is string => entry !== null);
    default:
      return [];
  }
}

export function bindingFor8(control: E8Control): E8Binding {
  const route = rawRoute(control.id);
  const e5 = bindingFor(control as never);
  const kind = route.kind;
  const input: E8InputKind =
    kind === 'missing'
      ? 'readonly'
      : kind === 'host'
        ? 'action'
        : route.kind === 'config'
          ? e5.input
          : route.kind === 'read'
            ? 'readonly'
            : e5.input === 'readonly'
              ? 'action'
              : e5.input;
  return {
    controlId: control.id,
    route,
    input,
    configKey: route.configKey,
    options: e5.options,
    optionsSource: e5.optionsSource,
    actionLabel: labelFor(control, kind),
    placeholder:
      input === 'secret-ref'
        ? 'env:NAME or vault:NAME (never a secret value)'
        : input === 'text' || input === 'textarea' || input === 'money'
          ? kind === 'missing'
            ? 'Unavailable'
            : input === 'money'
              ? 'Exact amount, e.g. 5.00'
              : 'Enter a value'
          : null,
    min: route.kind === 'config' && e5.input === 'number' ? (control.id === 'G-04' || control.id === 'G-05' ? 0 : e5.min) : null,
    max: route.kind === 'config' && e5.input === 'number' ? (control.id === 'G-04' ? 2 : control.id === 'G-05' ? 1 : e5.max) : null,
    step: route.kind === 'config' && e5.input === 'number' ? (control.id === 'G-04' || control.id === 'G-05' ? 0.1 : e5.step) : null,
    unit: e5.unit,
    // A toggle is a boolean control. Edition 5 also carries display options
    // ('on'/'off'); passing that option string to the toggle parser made the
    // real control look unavailable. The checkbox handler already stages a
    // boolean, so the harness must stage the same type.
    sample: input === 'toggle' ? true : e5.sample,
    kind,
  };
}

export const E8_BINDINGS: ReadonlyMap<string, E8Binding> = new Map(
  E8_CONTROLS.map((control) => [control.id, bindingFor8(control)]),
);

export function bindingOf8(controlId: string): E8Binding {
  const binding = E8_BINDINGS.get(controlId);
  if (binding === undefined) throw new Error(`edition8: no binding for ${controlId}`);
  return binding;
}

export function assertBindingsComplete(): readonly string[] {
  const missing: string[] = [];
  for (const control of E8_CONTROLS) {
    const binding = E8_BINDINGS.get(control.id);
    if (binding === undefined) {
      missing.push(control.id);
      continue;
    }
    try {
      rawRoute(control.id);
    } catch {
      missing.push(control.id);
    }
  }
  return missing;
}

export function sampleValue8(binding: E8Binding, options: readonly string[] = []): string | number | boolean {
  if (binding.input === 'select') return options.length > 0 ? options[0] : binding.sample;
  return binding.sample;
}

export function optionsForBinding(binding: E8Binding, snapshot: E8Snapshot | null): readonly string[] {
  if (binding.options.length > 0) return binding.options;
  return optionsFor(
    E8_CONTROLS.find((control) => control.id === binding.controlId) as E8Control,
    binding.optionsSource,
    snapshot,
  );
}

export function resolvedRoute(binding: E8Binding, snapshot: E8Snapshot | null, payload: Readonly<Record<string, Json>>): {
  readonly route: E8Route;
  readonly unresolved: string | null;
} {
  return resolveRoute(binding.controlId, { payload, snapshot });
}
