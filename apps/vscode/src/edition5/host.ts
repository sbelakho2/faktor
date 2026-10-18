// Pure controller between the panel/webview and the typed Edition 5 backend.
// This module owns staging, activation, atomic apply and served-view diffs;
// it is driven directly by the exercise and acceptance harnesses.

import { E5_CONTROLS } from '../generated/edition5Registry.ts';
import { E5_BINDINGS, GENERATION_PARAMETER_KEYS, bindingOf, type E5CommandKind } from './bindings.ts';
import {
  commandCall,
  defaultPayload,
  type E5Payload,
} from './commands.ts';
import type { E5Backend, E5ActionResult } from './client.ts';
import type { E5ConfigPatchResult, E5Snapshot, Json } from './contract.ts';
import {
  EMPTY_DRAFT,
  buildPatch,
  controlView,
  optionsFor,
  parseControlValue,
  resetDraft,
  screenView,
  stageControl,
  unstageControl,
  type E5ControlView,
  type E5Draft,
  type E5Row,
  type E5StagedChange,
  MUTATING_COMMANDS,
  ageText,
} from './view.ts';

export interface E5ControllerState {
  readonly snapshot: E5Snapshot | null;
  readonly draft: E5Draft;
  readonly loading: boolean;
  readonly error: string | null;
}

export const INITIAL_CONTROLLER_STATE: E5ControllerState = {
  snapshot: null,
  draft: EMPTY_DRAFT,
  loading: false,
  error: null,
};

export type E5ActivationKind = 'staged' | 'action' | 'read' | 'refused' | 'host';

export interface E5ActivationResult {
  readonly kind: E5ActivationKind;
  readonly controlId: string;
  readonly command: E5CommandKind;
  readonly draft: E5Draft;
  readonly message: string;
  readonly error: string | null;
  readonly action: E5ActionResult | null;
  readonly call: { readonly method: string; readonly path: string } | null;
}

function controlById(id: string) {
  const control = E5_CONTROLS.find((entry) => entry.id === id);
  if (control === undefined) throw new Error(`edition5: unknown control ${id}`);
  return control;
}

/** The representative value for a control (served choice first). */
export function sampleFor(controlId: string, snapshot: E5Snapshot | null): string | number | boolean {
  const binding = bindingOf(controlId);
  const options = optionsFor(binding, snapshot);
  if (binding.input === 'select') {
    if (options.length > 0) return options[0];
    return binding.sample;
  }
  return binding.sample;
}

function refuse(controlId: string, message: string): E5ActivationResult {
  return {
    kind: 'refused',
    controlId,
    command: bindingOf(controlId).command,
    draft: EMPTY_DRAFT,
    message,
    error: message,
    action: null,
    call: null,
  };
}

/** Stage one raw value; validation never writes to the daemon. */
export function stageRaw(
  state: E5ControllerState,
  controlId: string,
  raw: string | boolean | number | null,
  useSample: boolean,
): { readonly state: E5ControllerState; readonly error: string | null; readonly entry: E5StagedChange | null } {
  const binding = bindingOf(controlId);
  const value = raw === null || useSample ? sampleFor(controlId, state.snapshot) : raw;
  const staged = stageControl(state.draft, binding, value);
  if (staged.error !== null) {
    return { state, error: staged.error, entry: null };
  }
  const entry = staged.draft.set.get(controlId) ?? null;
  return {
    state: { ...state, draft: staged.draft },
    error: null,
    entry,
  };
}

export function unstage(
  state: E5ControllerState,
  controlId: string,
): E5ControllerState {
  return { ...state, draft: unstageControl(state.draft, bindingOf(controlId)) };
}

/** Restore defaults edits the form until Apply: every config key is unset. */
export function restoreDefaults(state: E5ControllerState): E5ControllerState {
  let draft: E5Draft = { set: new Map(), unset: new Set() };
  for (const control of E5_CONTROLS) {
    const binding = E5_BINDINGS.get(control.id);
    if (binding !== undefined && binding.configKey !== null) {
      draft = unstageControl(draft, binding);
    }
  }
  return { ...state, draft };
}

/** Activate one control: stage config, execute an action, or read state. */
export async function activateControl(
  backend: E5Backend,
  state: E5ControllerState,
  controlId: string,
  extra: E5Payload = {},
): Promise<{ readonly state: E5ControllerState; readonly result: E5ActivationResult }> {
  const control = controlById(controlId);
  const binding = bindingOf(controlId);
  const view: E5ControlView = controlView(control, state.snapshot, false, state.error);
  if (view.state === 'disabled' || view.state === 'error' || view.state === 'loading') {
    return { state, result: refuse(controlId, view.stateText) };
  }
  if (binding.input === 'readonly' || binding.command === 'inspect') {
    try {
      const snapshot = await backend.read(state.snapshot?.binding?.session ?? null);
      const result: E5ActivationResult = {
        kind: 'read',
        controlId,
        command: binding.command,
        draft: state.draft,
        message: 'Served state re-read.',
        error: null,
        action: null,
        call: { method: 'GET', path: '/native/ui/edition5' },
      };
      return { state: { ...state, snapshot, error: null, loading: false }, result };
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      return {
        state: { ...state, error: message },
        result: {
          kind: 'refused',
          controlId,
          command: binding.command,
          draft: state.draft,
          message,
          error: message,
          action: null,
          call: { method: 'GET', path: '/native/ui/edition5' },
        },
      };
    }
  }
  if (binding.command === 'host.action') {
    return {
      state,
      result: {
        kind: 'host',
        controlId,
        command: binding.command,
        draft: state.draft,
        message: 'Native VS Code action requested.',
        error: null,
        action: null,
        call: null,
      },
    };
  }
  if (binding.command !== 'config.set') {
    if (state.snapshot?.stale === true && MUTATING_COMMANDS.has(binding.command)) {
      return {
        state,
        result: refuse(
          controlId,
          'This action changes daemon state while the observations are stale; reconcile the binding before dispatching.',
        ),
      };
    }
    const payload: Record<string, Json> = {
      ...defaultPayload(binding.command, state.snapshot),
      ...extra,
    };
    const value = extra.value;
    if (controlId === 'M-01' && value !== undefined) {
      payload.provider = value;
      if (payload.model === undefined) {
        const current = state.snapshot?.config?.values.find((entry) => entry.key === 'faktor.defaultModel')?.value;
        if (typeof current === 'string') payload.model = current;
      }
    }
    if (controlId === 'M-02' && value !== undefined) {
      payload.model = value;
      if (payload.provider === undefined) {
        const current = state.snapshot?.config?.values.find((entry) => entry.key === 'faktor.defaultProvider')?.value;
        if (typeof current === 'string') payload.provider = current;
      }
    }
    if (GENERATION_PARAMETER_KEYS[controlId] !== undefined && value !== undefined) {
      payload.key = GENERATION_PARAMETER_KEYS[controlId];
      payload.value = value;
    }
    if (typeof payload.session !== 'string' && state.snapshot?.binding?.session != null) {
      payload.session = state.snapshot.binding.session;
    }
    let call: { method: string; path: string } | null = null;
    try {
      const planned = commandCall(binding.command, payload);
      call = { method: planned.method, path: planned.path };
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      return { state, result: refuse(controlId, message) };
    }
    const action = await backend.execute(binding.command, payload);
    let next: E5ControllerState = state;
    if (action.ok) {
      try {
        const snapshot = await backend.read(state.snapshot?.binding?.session ?? null);
        next = { ...state, snapshot, error: null, loading: false };
      } catch (error) {
        next = { ...state, error: error instanceof Error ? error.message : String(error) };
      }
    }
    return {
      state: next,
      result: {
        kind: action.ok ? 'action' : 'refused',
        controlId,
        command: binding.command,
        draft: state.draft,
        message: action.message,
        error: action.ok ? null : action.message,
        action,
        call,
      },
    };
  }
  const staged = stageRaw(state, controlId, null, true);
  if (staged.error !== null) {
    return { state, result: refuse(controlId, staged.error) };
  }
  return {
    state: staged.state,
    result: {
      kind: 'staged',
      controlId,
      command: binding.command,
      draft: staged.state.draft,
      message: 'Staged locally; Apply sends one atomic patch against the current revision.',
      error: null,
      action: null,
      call: { method: 'POST', path: '/native/config/patch' },
    },
  };
}

export interface E5ApplyOutcome {
  readonly state: E5ControllerState;
  readonly patch: E5ConfigPatchResult | null;
  readonly message: string;
  readonly ok: boolean;
}

/** One validated atomic patch; a refusal writes nothing and keeps the draft. */
export async function applyDraft(
  backend: E5Backend,
  state: E5ControllerState,
  set: readonly E5StagedChange[],
  unset: readonly string[],
): Promise<E5ApplyOutcome> {
  const revision = state.snapshot?.config?.revision ?? null;
  if (revision === null) {
    return {
      state,
      patch: null,
      ok: false,
      message: 'The configuration revision is unavailable; Apply stays disabled until the daemon read succeeds.',
    };
  }
  if (state.snapshot?.stale === true) {
    return {
      state,
      patch: null,
      ok: false,
      message: 'Apply is disabled while the observations are stale. Reconcile the session first.',
    };
  }
  const localErrors: string[] = [];
  for (const entry of set) {
    const binding = [...E5_BINDINGS.values()].find((candidate) => candidate.controlId === entry.controlId);
    if (binding === undefined) continue;
    const parsed = parseControlValue(binding, entry.value as string | boolean | number);
    if (parsed.error !== null) localErrors.push(`${entry.controlId}: ${parsed.error}`);
  }
  if (localErrors.length > 0) {
    return { state, patch: null, ok: false, message: localErrors.join(' ') };
  }
  const patch = buildPatch({ set: new Map(set.map((entry) => [entry.controlId, entry])), unset: new Set(unset) }, revision);
  const result = await backend.applyPatch({
    expected_revision: revision,
    set: patch.patch.set.map((entry) => ({
      key: entry.key,
      value: entry.value,
      control_id: entry.controlId,
    })),
    unset: patch.patch.unset,
  });
  if (!result.applied || result.errors.length > 0) {
    return {
      state,
      patch: result,
      ok: false,
      message:
        result.errors.length > 0
          ? result.errors.map((error) => `${error.key} (${error.code}): ${error.message}`).join(' ')
          : 'The daemon refused the patch; nothing was written.',
    };
  }
  let snapshot = state.snapshot;
  try {
    snapshot = await backend.read(state.snapshot?.binding?.session ?? null);
  } catch {
    // The patch is acknowledged; a failed re-read keeps the last coherent
    // state and the next refresh reconciles it. It never invents success.
  }
  return {
    state: { snapshot, draft: resetDraft(), error: null, loading: false },
    patch: result,
    ok: true,
    message: `Saved revision ${result.revision}; per-field apply states are shown on each control.`,
  };
}

// ----------------------------------------------------------- view diffing

export interface E5ControlPatch {
  readonly id: string;
  readonly state: string;
  readonly stateText: string;
  readonly valueText: string;
  readonly source: string | null;
  readonly disabled: boolean;
  readonly options: readonly string[];
  readonly applyState: string | null;
  readonly value: Json;
}

export interface E5ViewPatch {
  readonly revision: string | null;
  readonly stale: boolean;
  readonly staleText: string | null;
  readonly controls: readonly E5ControlPatch[];
  readonly rows: Readonly<Record<string, readonly E5Row[]>>;
  readonly notice: string | null;
}

export interface E5ViewCache {
  readonly controls: ReadonlyMap<string, string>;
  readonly rows: ReadonlyMap<string, string>;
}

export const EMPTY_VIEW_CACHE: E5ViewCache = { controls: new Map(), rows: new Map() };

export function controlPatchOf(view: E5ControlView): E5ControlPatch {
  return {
    id: view.control.id,
    state: view.state,
    stateText: view.stateText,
    valueText: view.valueText,
    source: view.source,
    disabled: view.disabled,
    options: view.options,
    applyState: view.applyState,
    value: view.configValue,
  };
}

function controlSignature(view: E5ControlView): string {
  return JSON.stringify(controlPatchOf(view));
}

function rowsSignature(rows: readonly E5Row[]): string {
  return JSON.stringify(rows);
}

/**
 * Compute the minimum DOM delta: one entry per changed control and one full
 * keyed row list per changed screen. The scan is O(controls + rows); the
 * webview touches only the returned delta (its probe counts each node).
 */
export function diffView(
  snapshot: E5Snapshot | null,
  cache: E5ViewCache,
  notice: string | null,
): { readonly patch: E5ViewPatch; readonly cache: E5ViewCache } {
  const controls: E5ControlPatch[] = [];
  const nextControls = new Map<string, string>();
  for (const [id, binding] of E5_BINDINGS) {
    const control = controlById(id);
    const view = controlView(control, snapshot, false, null);
    const signature = controlSignature(view);
    nextControls.set(id, signature);
    if (cache.controls.get(id) !== signature) {
      controls.push(controlPatchOf(view));
    }
    // Keep the binding referenced so the scan stays the registry authority.
    void binding.command;
  }
  const rowPatches: Record<string, readonly E5Row[]> = {};
  const nextRows = new Map<string, string>();
  for (const screen of screensWithRows(snapshot)) {
    const signature = rowsSignature(screen.rows);
    nextRows.set(screen.id, signature);
    if (cache.rows.get(screen.id) !== signature) {
      rowPatches[screen.id] = screen.rows;
    }
  }
  return {
    patch: {
      revision: snapshot?.config?.revision ?? null,
      stale: snapshot?.stale === true,
      staleText:
        snapshot?.stale === true
          ? `Observations are stale by ${ageText(snapshot.stale_age_ms)}; admission and apply are disabled until reconciled.`
          : null,
      controls,
      rows: rowPatches,
      notice,
    },
    cache: { controls: nextControls, rows: nextRows },
  };
}

function screensWithRows(snapshot: E5Snapshot | null): readonly { readonly id: string; readonly rows: readonly E5Row[] }[] {
  return [...E5_CONTROLS.reduce((set, control) => {
    for (const screen of control.screens) set.add(screen);
    return set;
  }, new Set<string>())]
    .sort()
    .map((id) => ({
      id,
      rows: screenView(id, { snapshot, loading: false, error: null, focusControl: null }).rows,
    }));
}

/** The set of controls the panel can act on, for the exercise registry. */
export function allBindings(): readonly string[] {
  return [...E5_BINDINGS.keys()];
}
