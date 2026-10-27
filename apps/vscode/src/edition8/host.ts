// Pure controller between the panel/webview and the typed Edition 8 backend.
// It owns staging, activation, atomic apply and keyed view diffs; the
// exercise/workflow/acceptance harnesses drive exactly this module.

import { E8_CONTROLS } from '../generated/edition8Registry.ts';
import { bindingOf8, type E8Binding } from './bindings.ts';
import type { E8ActionResult, E8Backend } from './client.ts';
import { configValues } from './source.ts';
import type { E8ConfigValue, E8Snapshot, Json } from './contract.ts';
import {
  controlView,
  screenView,
  type E8ControlView,
  type E8PanelState,
  type E8ScreenView,
} from './view.ts';

export interface E8StagedChange {
  readonly controlId: string;
  readonly key: string;
  readonly value: Json;
  readonly scope: string;
}

export interface E8Draft {
  readonly set: ReadonlyMap<string, E8StagedChange>;
  readonly unset: ReadonlySet<string>;
}

export const EMPTY_DRAFT: E8Draft = { set: new Map(), unset: new Set() };

export interface E8ControllerState {
  readonly snapshot: E8Snapshot | null;
  readonly draft: E8Draft;
  readonly loading: boolean;
  readonly error: string | null;
}

export const INITIAL_CONTROLLER_STATE: E8ControllerState = {
  snapshot: null,
  draft: EMPTY_DRAFT,
  loading: false,
  error: null,
};

export type E8ActivationKind = 'staged' | 'action' | 'read' | 'refused' | 'host';

export interface E8ActivationResult {
  readonly kind: E8ActivationKind;
  readonly controlId: string;
  readonly draft: E8Draft;
  readonly message: string;
  readonly error: string | null;
  readonly action: E8ActionResult | null;
  readonly call: { readonly method: string; readonly path: string } | null;
}

const CONTROL_BY_ID = new Map<string, (typeof E8_CONTROLS)[number]>();
for (const entry of E8_CONTROLS) {
  // FIRST occurrence, exactly the historical `find` result.
  if (!CONTROL_BY_ID.has(entry.id)) CONTROL_BY_ID.set(entry.id, entry);
}

function controlById(id: string) {
  const control = CONTROL_BY_ID.get(id);
  if (control === undefined) throw new Error(`edition8: unknown control ${id}`);
  return control;
}

export function sampleFor(controlId: string, snapshot: E8Snapshot | null): string | number | boolean {
  const binding = bindingOf8(controlId);
  if (binding.input === 'select') {
    const view = controlView(controlById(controlId), snapshot, false, null);
    if (view.options.length > 0) return view.options[0];
  }
  return binding.sample;
}

function stageable(binding: E8Binding): boolean {
  return binding.route.kind === 'config' && binding.configKey !== null && binding.input !== 'action';
}

export function stageRaw(
  state: E8ControllerState,
  controlId: string,
  raw: string | boolean | number | null,
  useSample: boolean,
): { readonly state: E8ControllerState; readonly error: string | null; readonly entry: E8StagedChange | null } {
  const binding = bindingOf8(controlId);
  if (!stageable(binding)) {
    return { state, error: `${controlId} is not a staged configuration control.`, entry: null };
  }
  const value = raw === null || useSample ? sampleFor(controlId, state.snapshot) : raw;
  const parsed = parseValue(binding, value);
  if (parsed.error !== null) return { state, error: parsed.error, entry: null };
  const entry: E8StagedChange = {
    controlId,
    key: binding.configKey as string,
    value: parsed.value,
    scope: 'user',
  };
  const set = new Map(state.draft.set);
  set.set(controlId, entry);
  const unset = new Set(state.draft.unset);
  unset.delete(controlId);
  return { state: { ...state, draft: { set, unset } }, error: null, entry };
}

export function unstage(state: E8ControllerState, controlId: string): E8ControllerState {
  const binding = bindingOf8(controlId);
  if (binding.configKey === null) return state;
  const set = new Map(state.draft.set);
  set.delete(controlId);
  const unset = new Set(state.draft.unset);
  unset.add(controlId);
  return { ...state, draft: { set, unset } };
}

/** Restore defaults stages removals; nothing is written until Apply. */
export function restoreDefaults(state: E8ControllerState): E8ControllerState {
  const unset = new Set<string>();
  for (const control of E8_CONTROLS) {
    const binding = bindingOf8(control.id);
    if (binding.route.kind === 'config' && binding.configKey !== null) unset.add(control.id);
  }
  return { ...state, draft: { set: new Map(), unset } };
}

export function clearDraft(state: E8ControllerState): E8ControllerState {
  return { ...state, draft: EMPTY_DRAFT };
}

function parseValue(binding: E8Binding, raw: string | boolean | number): { value: Json; error: string | null } {
  switch (binding.input) {
    case 'toggle':
      if (typeof raw !== 'boolean') return { value: null, error: 'Expected on/off.' };
      return { value: raw, error: null };
    case 'number': {
      const value = typeof raw === 'number' ? raw : Number(raw);
      const integral = binding.step === null || binding.step >= 1;
      if (!Number.isFinite(value) || (integral && !Number.isInteger(value))) {
        return { value: null, error: integral ? 'Enter a whole number; the value was not changed.' : 'Enter a finite number; the value was not changed.' };
      }
      if (binding.min !== null && value < binding.min) return { value: null, error: `The minimum is ${binding.min}.` };
      if (binding.max !== null && value > binding.max) return { value: null, error: `The maximum is ${binding.max}.` };
      return { value, error: null };
    }
    case 'money': {
      const text = String(raw);
      if (!/^\d+(\.\d{1,6})?$/.test(text)) {
        return { value: null, error: 'Enter an exact non-negative USD amount with at most 6 decimals.' };
      }
      const [whole, fraction = ''] = text.split('.');
      const micro = `${whole}${fraction.padEnd(6, '0')}`.replace(/^0+(?=\d)/, '');
      if (!/^\d+$/.test(micro) || BigInt(micro) > BigInt('9223372036854775807')) {
        return { value: null, error: 'The amount overflows the exact micro-USD range.' };
      }
      return { value: micro, error: null };
    }
    case 'list':
      return {
        value: String(raw)
          .split(/\r?\n/)
          .map((entry) => entry.trim())
          .filter((entry) => entry.length > 0),
        error: null,
      };
    default:
      return { value: typeof raw === 'string' ? raw : String(raw), error: null };
  }
}

export function buildPatch(
  draft: E8Draft,
  expectedRevision: string,
): {
  readonly patch: {
    readonly expected_revision: string;
    readonly set: readonly E8StagedChange[];
    readonly unset: readonly string[];
  };
  readonly controlIds: readonly string[];
} {
  const set = [...draft.set.values()];
  const unset = [...draft.unset];
  return {
    patch: { expected_revision: expectedRevision, set, unset },
    controlIds: [...set.map((entry) => entry.controlId), ...unset],
  };
}

export interface E8ApplyOutcome {
  readonly state: E8ControllerState;
  readonly ok: boolean;
  readonly message: string;
  readonly applied: boolean;
  readonly fieldStates: readonly { readonly key: string; readonly state: string }[];
}

export async function applyDraft(
  backend: E8Backend,
  state: E8ControllerState,
  set: readonly E8StagedChange[],
  unset: readonly string[],
): Promise<E8ApplyOutcome> {
  const snapshot = state.snapshot;
  if (snapshot === null) {
    return { state, ok: false, applied: false, message: 'No served configuration revision is loaded; the draft is preserved.', fieldStates: [] };
  }
  const revision = typeof snapshot.config === 'object' && snapshot.config !== null && 'revision' in snapshot.config
    ? String((snapshot.config as { revision?: Json }).revision ?? '0')
    : '0';
  const changes = [
    ...set.map((entry) => ({ key: entry.key, value: entry.value, scope: entry.scope })),
    ...unset
      .map((controlId) => bindingOf8(controlId).configKey)
      .filter((key): key is string => key !== null)
      .map((key) => ({ key, value: undefined, scope: 'user' })),
  ];
  const result = await backend.applyPatch(revision, changes);
  if (!result.ok) {
    return {
      state: { ...state, error: result.message },
      ok: false,
      applied: false,
      message: `${result.message} The staged draft and its revision are preserved.`,
      fieldStates: result.field_states,
    };
  }
  let next = state;
  try {
    const refreshed = await backend.read({
      session: snapshot.binding?.session ?? null,
      workspace: snapshot.binding?.workspace ?? null,
      worktree: snapshot.binding?.worktree ?? null,
      daemon: snapshot.binding?.daemon ?? null,
      generation: snapshot.binding?.generation ?? null,
      previous: snapshot,
    });
    next = { ...state, snapshot: refreshed, draft: EMPTY_DRAFT, error: null };
  } catch (error) {
    next = {
      ...state,
      draft: EMPTY_DRAFT,
      error: error instanceof Error ? error.message : String(error),
    };
  }
  return {
    state: next,
    ok: true,
    applied: true,
    message: result.message,
    fieldStates: result.field_states,
  };
}

/** Activate one control: stage config, execute an action, or re-read state. */
export async function activateControl(
  backend: E8Backend,
  state: E8ControllerState,
  controlId: string,
  extra: Readonly<Record<string, Json>> = {},
): Promise<{ readonly state: E8ControllerState; readonly result: E8ActivationResult }> {
  const binding = bindingOf8(controlId);
  const view = controlView(controlById(controlId), state.snapshot, false, state.error);
  if (view.state === 'disabled' || view.state === 'error' || view.state === 'loading' || view.state === 'stale') {
    return {
      state,
      result: {
        kind: 'refused',
        controlId,
        draft: state.draft,
        message: view.stateText,
        error: view.stateText,
        action: null,
        call: { method: binding.route.method, path: binding.route.path },
      },
    };
  }
  if (stageable(binding)) {
    const staged = stageRaw(state, controlId, null, true);
    if (staged.error !== null) {
      return {
        state,
        result: {
          kind: 'refused',
          controlId,
          draft: state.draft,
          message: staged.error,
          error: staged.error,
          action: null,
          call: { method: 'POST', path: '/native/config/commit' },
        },
      };
    }
    return {
      state: staged.state,
      result: {
        kind: 'staged',
        controlId,
        draft: staged.state.draft,
        message: `${controlId} staged locally; Apply sends one atomic patch against the current revision.`,
        error: null,
        action: null,
        call: { method: 'POST', path: '/native/config/commit' },
      },
    };
  }
  if (binding.route.kind === 'host') {
    return {
      state,
      result: {
        kind: 'host',
        controlId,
        draft: state.draft,
        message: binding.route.reason ?? 'Native host action requested.',
        error: null,
        action: null,
        call: null,
      },
    };
  }
  if (binding.route.kind === 'missing') {
    return {
      state,
      result: {
        kind: 'refused',
        controlId,
        draft: state.draft,
        message: binding.route.reason ?? 'This capability is not served by the daemon.',
        error: binding.route.reason ?? 'This capability is not served by the daemon.',
        action: null,
        call: { method: binding.route.method, path: binding.route.path },
      },
    };
  }
  const action = await backend.execute(controlId, extra, state.snapshot);
  let next = state;
  if (action.ok && binding.route.kind !== 'read') {
    try {
      const refreshed = await backend.read({
        session: state.snapshot?.binding?.session ?? null,
        workspace: state.snapshot?.binding?.workspace ?? null,
        worktree: state.snapshot?.binding?.worktree ?? null,
        daemon: state.snapshot?.binding?.daemon ?? null,
        generation: state.snapshot?.binding?.generation ?? null,
        previous: state.snapshot,
      });
      next = { ...state, snapshot: refreshed, error: null };
    } catch (error) {
      next = { ...state, error: error instanceof Error ? error.message : String(error) };
    }
  } else if (action.ok && binding.route.kind === 'read') {
    try {
      const refreshed = await backend.read({
        session: state.snapshot?.binding?.session ?? null,
        workspace: state.snapshot?.binding?.workspace ?? null,
        worktree: state.snapshot?.binding?.worktree ?? null,
        daemon: state.snapshot?.binding?.daemon ?? null,
        generation: state.snapshot?.binding?.generation ?? null,
        previous: state.snapshot,
      });
      next = { ...state, snapshot: refreshed, error: null };
    } catch (error) {
      next = { ...state, error: error instanceof Error ? error.message : String(error) };
    }
  }
  return {
    state: next,
    result: {
      kind: action.ok ? (binding.route.kind === 'read' ? 'read' : 'action') : 'refused',
      controlId,
      draft: next.draft,
      message: action.message,
      error: action.ok ? null : action.message,
      action,
      call: { method: action.method, path: action.path },
    },
  };
}

// ------------------------------------------------------------ keyed diffs

export interface E8ControlPatch {
  readonly id: string;
  readonly state: string;
  readonly stateText: string;
  readonly valueText: string;
  readonly disabled: boolean;
  readonly signature: string;
  /** Served choices for a picker; omitted for non-choice controls. */
  readonly options?: readonly string[];
  /** Raw served configuration value (the picker's selected value). */
  readonly value?: Json | null;
}

export interface E8ViewPatch {
  readonly kind: 'e8Patch';
  readonly screen: string;
  readonly controls: readonly E8ControlPatch[];
  readonly rowsSignature: string;
  readonly rows?: readonly { readonly key: string; readonly label: string; readonly value: string; readonly state: string }[];
  readonly status: string;
  readonly statusLevel: string;
  readonly stale: boolean;
}

function signatureOf(view: E8ControlView): string {
  return [view.state, view.stateText, view.valueText, view.disabled, view.applyState ?? '', view.options.join(',')].join('|');
}

function rowsSignature(view: E8ScreenView): string {
  return view.rows.map((row) => `${row.key}=${row.state}:${row.value}`).join('|');
}

export interface E8ViewCache {
  readonly controls: ReadonlyMap<string, string>;
  readonly rows: ReadonlyMap<string, string>;
  readonly views?: ReadonlyMap<string, E8ScreenView>;
  readonly dataRevision?: string;
  readonly stateKey?: string;
}

export const EMPTY_VIEW_CACHE: E8ViewCache = { controls: new Map(), rows: new Map() };

const VOLATILE_SCREENS = new Set(['S56']);

/** Only changed controls/rows are posted; unchanged cards keep their DOM. */
export function diffView(
  state: E8PanelState,
  screenIds: readonly string[],
  cache: E8ViewCache,
): { readonly cache: E8ViewCache; readonly patches: readonly E8ViewPatch[] } {
  const controlSignatures = new Map<string, string>();
  const rowSignatures = new Map<string, string>();
  const patches: E8ViewPatch[] = [];
  const revision = state.snapshot?.data_revision ?? null;
  const stateKey = [
    state.loading ? 'loading' : 'idle',
    state.error ?? '',
    Math.floor((state.snapshot?.stale_age_ms ?? 0) / 1000),
  ].join('|');
  const reuse = revision !== null && cache.dataRevision === revision && cache.stateKey === stateKey;
  const views = new Map<string, E8ScreenView>(reuse ? cache.views : undefined);
  for (const screenId of screenIds) {
    const cachedView = reuse && !VOLATILE_SCREENS.has(screenId) ? cache.views?.get(screenId) : undefined;
    const view = cachedView ?? screenView(screenId, state);
    views.set(screenId, view);
    const controls: E8ControlPatch[] = [];
    for (const control of view.controls) {
      const signature = signatureOf(control);
      const previous = cache.controls.get(`${screenId}:${control.control.id}`);
      controlSignatures.set(`${screenId}:${control.control.id}`, signature);
      if (previous !== signature) {
        controls.push({
          id: control.control.id,
          state: control.state,
          stateText: control.stateText,
          valueText: control.valueText,
          disabled: control.disabled,
          signature,
          ...(control.options.length > 0 ? { options: control.options } : {}),
          value: control.configValue,
        });
      }
    }
    const rows = rowsSignature(view);
    const signature = `${rows}::${view.status}|${view.statusLevel}|${view.stale}`;
    const rowsChanged = cache.rows.get(screenId) !== signature;
    rowSignatures.set(screenId, signature);
    if (!rowsChanged && controls.length === 0) continue;
    patches.push({
      kind: 'e8Patch',
      screen: screenId,
      controls,
      rowsSignature: rows,
      rows: rowsChanged ? view.rows.map((entry) => ({ key: entry.key, label: entry.label, value: entry.value, state: entry.state })) : undefined,
      status: view.status,
      statusLevel: view.statusLevel,
      stale: view.stale,
    });
  }
  return {
    cache: {
      controls: controlSignatures,
      rows: rowSignatures,
      views,
      ...(revision === null ? {} : { dataRevision: revision }),
      stateKey,
    },
    patches,
  };
}

export function allControlViews(snapshot: E8Snapshot | null): readonly E8ControlView[] {
  return E8_CONTROLS.map((control) => controlView(control, snapshot, false, null));
}

export function controlledConfig(snapshot: E8Snapshot | null): readonly E8ConfigValue[] {
  return configValues(snapshot?.config ?? null);
}
