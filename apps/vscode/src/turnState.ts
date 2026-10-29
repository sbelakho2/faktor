// One reducer for the turn lifecycle (audit item: the Ask path cleared
// busy/activeRunId on HTTP ACCEPTANCE, so the UI showed Ready with no Stop
// while the reply was still generating).
//
// States (exactly one per tracked submission):
//
//   idle → submitting → queued | starting → streaming | tool-running |
//   awaiting-user → completed | interrupted | failed
//
// Rules the reducer enforces:
//   * a receipt (op/run ids) moves submitting → queued or starting — never
//     straight to a terminal state (HTTP 2xx is admission, not completion);
//   * terminal transitions come ONLY from durable authority: the turn
//     record status (`turnStatus`), the durable journal's `turn_completed`
//     / `failed` events, or the typed abort ack (`stopped`) — never from a
//     quiet stream;
//   * a machine-state tag maps to a NON-terminal phase only (a quiet or
//     ready machine never completes a turn by itself);
//   * terminal entries are sticky until a NEW submit supersedes them, and
//     the entry list is bounded (stale terminal entries are pruned).
//
// Pure module: no vscode import, no clock, no I/O — the selftest drives
// every transition directly.

export type TurnPhase =
  | 'idle'
  | 'submitting'
  | 'queued'
  | 'starting'
  | 'streaming'
  | 'tool-running'
  | 'awaiting-user'
  | 'completed'
  | 'interrupted'
  | 'failed';

export type TurnSource = 'ask' | 'task';

export interface TurnEntry {
  /** The admission receipt's operation id (null until the receipt lands). */
  opId: string | null;
  runId: string | null;
  readonly source: TurnSource;
  phase: TurnPhase;
}

export interface TurnLifecycle {
  /** Bounded: newest submission last (MAX_TURNS, oldest terminals pruned). */
  entries: TurnEntry[];
}

/** Hard bound of tracked submissions (bounded everything). */
export const MAX_TURNS = 4;

const TERMINAL_PHASES: readonly TurnPhase[] = ['completed', 'interrupted', 'failed'];

const LIVE_PHASES: readonly TurnPhase[] = [
  'submitting',
  'queued',
  'starting',
  'streaming',
  'tool-running',
  'awaiting-user',
];

/** Phases in which Stop is meaningful (an operation exists and is live). */
const STOPPABLE_PHASES: readonly TurnPhase[] = [
  'starting',
  'streaming',
  'tool-running',
  'awaiting-user',
];

export function emptyTurnLifecycle(): TurnLifecycle {
  return { entries: [] };
}

export function isTerminalPhase(phase: TurnPhase): boolean {
  return TERMINAL_PHASES.indexOf(phase) >= 0;
}

/** True while ANY tracked submission is unfinished (drives `busy`). */
export function turnLifecycleIsLive(state: TurnLifecycle): boolean {
  return state.entries.some((entry) => LIVE_PHASES.indexOf(entry.phase) >= 0);
}

/**
 * The operation Stop must abort: the FIRST non-terminal entry that has an
 * operation id and is actually running (a queued follow-up is never the
 * Stop target — the streaming turn ahead of it is).
 */
export function turnStopOpId(state: TurnLifecycle): string | null {
  for (const entry of state.entries) {
    if (STOPPABLE_PHASES.indexOf(entry.phase) >= 0 && entry.opId !== null) {
      return entry.opId;
    }
  }
  return null;
}

/**
 * The one display phase: the NEWEST submission's phase (a queued follow-up
 * therefore renders `queued` while an earlier turn still streams), else
 * the newest live entry, else the newest terminal outcome, else idle.
 */
export function turnDisplayPhase(state: TurnLifecycle): TurnPhase {
  if (state.entries.length === 0) {
    return 'idle';
  }
  const newest = state.entries[state.entries.length - 1];
  if (newest.phase !== 'idle') {
    return newest.phase;
  }
  for (let i = state.entries.length - 1; i >= 0; i -= 1) {
    const phase = state.entries[i].phase;
    if (phase !== 'idle') {
      return phase;
    }
  }
  return 'idle';
}

/** The product copy for one phase (null for idle: fall back to Ready). */
export function turnPhaseLabel(phase: TurnPhase): string | null {
  switch (phase) {
    case 'submitting':
      return 'Sending…';
    case 'queued':
      return 'Queued';
    case 'starting':
      return 'Starting…';
    case 'streaming':
      return 'Generating…';
    case 'tool-running':
      return 'Working…';
    case 'awaiting-user':
      return 'Needs approval';
    case 'completed':
      return 'Completed';
    case 'interrupted':
      return 'Stopped';
    case 'failed':
      return 'Failed';
    case 'idle':
      return null;
    default:
      return null;
  }
}

export type TurnEvent =
  | { readonly type: 'submit'; readonly source: TurnSource }
  | {
      readonly type: 'receipt';
      readonly opId: string | null;
      readonly runId?: string | null;
      readonly queued: boolean;
    }
  | { readonly type: 'admitted'; readonly opId: string }
  | { readonly type: 'machine'; readonly state: string; readonly opId?: string }
  | {
      readonly type: 'journal';
      readonly kind: string;
      readonly opId?: string | null;
      readonly state?: string;
    }
  | { readonly type: 'turnStatus'; readonly opId: string | null; readonly status: string }
  | { readonly type: 'stopped'; readonly opId: string }
  | { readonly type: 'reset' };

/** AgentState tag → non-terminal phase (never a terminal transition). */
function phaseFromMachineState(state: string): TurnPhase | null {
  switch (state) {
    case 'preparing':
    case 'building_context':
      return 'starting';
    case 'waiting_for_model':
    case 'streaming':
      return 'streaming';
    case 'tool_requested':
    case 'executing_tool':
    case 'updating_memory':
    case 'validating':
      return 'tool-running';
    case 'waiting_for_permission':
    case 'needs_user_input':
    case 'suspended':
      return 'awaiting-user';
    default:
      // ready_for_next_turn / idle / terminal machine tags NEVER flip the
      // turn: completion comes from the durable turn record, not silence.
      return null;
  }
}

function prune(entries: TurnEntry[]): TurnEntry[] {
  if (entries.length <= MAX_TURNS) {
    return entries;
  }
  // Drop the OLDEST entries while over the bound; terminal entries first.
  const drop = entries.length - MAX_TURNS;
  const kept: TurnEntry[] = [];
  let dropped = 0;
  for (const entry of entries) {
    if (dropped < drop && isTerminalPhase(entry.phase)) {
      dropped += 1;
      continue;
    }
    kept.push(entry);
  }
  // Still over (all live): drop from the front (bounded regardless).
  while (kept.length > MAX_TURNS) {
    kept.shift();
  }
  return kept;
}

function setPhase(entry: TurnEntry, phase: TurnPhase): void {
  // Terminal outcomes are sticky for THAT entry: a late machine tag or a
  // stale frame can never resurrect a finished turn.
  if (isTerminalPhase(entry.phase)) {
    return;
  }
  entry.phase = phase;
}

function entryForOp(state: TurnLifecycle, opId: string | null): TurnEntry | null {
  if (opId !== null) {
    for (const entry of state.entries) {
      if (entry.opId === opId) {
        return entry;
      }
    }
    return null;
  }
  // No op id: the newest LIVE (non-terminal) entry.
  for (let i = state.entries.length - 1; i >= 0; i -= 1) {
    if (!isTerminalPhase(state.entries[i].phase) && state.entries[i].phase !== 'idle') {
      return state.entries[i];
    }
  }
  return null;
}

/** The one reducer: `turnReducer(state, event) -> state` (mutates a copy). */
export function turnReducer(state: TurnLifecycle, event: TurnEvent): TurnLifecycle {
  const entries: TurnEntry[] = state.entries.map((entry) => ({ ...entry }));
  const next: TurnLifecycle = { entries };
  switch (event.type) {
    case 'reset':
      return emptyTurnLifecycle();
    case 'submit': {
      entries.push({
        opId: null,
        runId: null,
        source: event.source,
        phase: 'submitting',
      });
      next.entries = prune(entries);
      return next;
    }
    case 'receipt': {
      // Attach the receipt to the LAST submission still awaiting it (the
      // receipt arrives before any status can possibly change it).
      for (let i = entries.length - 1; i >= 0; i -= 1) {
        if (entries[i].opId === null && entries[i].phase === 'submitting') {
          entries[i].opId = event.opId;
          entries[i].runId = event.runId ?? null;
          entries[i].phase = event.queued ? 'queued' : 'starting';
          break;
        }
      }
      return next;
    }
    case 'admitted': {
      const entry = entryForOp(next, event.opId);
      if (entry === null) {
        // Admission for an unknown op (another client's turn): adopt it as
        // the active entry only when the newest submission still awaits
        // its own admission.
        const newest = entries.length > 0 ? entries[entries.length - 1] : null;
        if (newest !== null && newest.opId === null) {
          newest.opId = event.opId;
          newest.phase = 'starting';
        } else if (newest !== null && newest.opId === event.opId) {
          setPhase(newest, 'starting');
        }
        return next;
      }
      if (entry.phase === 'queued' || entry.phase === 'submitting') {
        entry.phase = 'starting';
      }
      return next;
    }
    case 'machine': {
      const mapped = phaseFromMachineState(event.state);
      if (mapped === null) {
        return next;
      }
      const entry = entryForOp(next, event.opId ?? null) ?? entryForOp(next, null);
      if (entry !== null) {
        setPhase(entry, mapped);
      }
      return next;
    }
    case 'journal': {
      const opId = event.opId ?? null;
      switch (event.kind) {
        case 'prompt_admitted':
        case 'prompt_received': {
          const entry = opId !== null ? entryForOp(next, opId) : null;
          if (entry !== null) {
            if (entry.phase === 'queued' || entry.phase === 'submitting') {
              entry.phase = 'starting';
            }
            return next;
          }
          const newest = entries.length > 0 ? entries[entries.length - 1] : null;
          if (newest !== null && newest.opId === null && opId !== null) {
            newest.opId = opId;
            newest.phase = 'starting';
          } else if (newest !== null && newest.opId === opId && opId !== null) {
            setPhase(newest, 'starting');
          }
          return next;
        }
        case 'model_started': {
          const entry = entryForOp(next, opId);
          if (entry !== null) {
            setPhase(entry, 'streaming');
          }
          return next;
        }
        case 'phase_changed': {
          const mapped = event.state === undefined ? null : phaseFromMachineState(event.state);
          if (mapped === null) {
            return next;
          }
          const entry = entryForOp(next, opId);
          if (entry !== null) {
            setPhase(entry, mapped);
          }
          return next;
        }
        case 'tool_started':
        case 'tool_completed':
        case 'tool_result_appended':
        case 'memory_updated': {
          const entry = entryForOp(next, opId);
          if (entry !== null) {
            setPhase(entry, 'tool-running');
          }
          return next;
        }
        case 'turn_completed': {
          const entry = entryForOp(next, opId);
          if (entry !== null) {
            setPhase(entry, 'completed');
          }
          return next;
        }
        case 'failed': {
          const entry = entryForOp(next, opId);
          if (entry !== null) {
            setPhase(entry, 'failed');
          }
          return next;
        }
        default:
          return next;
      }
    }
    case 'turnStatus': {
      let entry = entryForOp(next, event.opId);
      if (entry === null) {
        if (event.status === 'active' && event.opId !== null && !turnLifecycleIsLive(next)) {
          // Durable authority reports an ACTIVE turn the local reducer does
          // not track (daemon restart / fresh session): adopt it instead of
          // showing idle while the daemon is still generating.
          next.entries = prune([
            ...next.entries,
            { opId: event.opId, runId: null, source: 'ask', phase: 'starting' },
          ]);
          return next;
        }
        return next;
      }
      switch (event.status) {
        case 'completed':
          setPhase(entry, 'completed');
          break;
        case 'cancelled':
          setPhase(entry, 'interrupted');
          break;
        case 'failed':
          setPhase(entry, 'failed');
          break;
        case 'active':
          // Durable authority says the turn IS running: a queued/submitted/
          // even wrongly-terminal display becomes live (never a completion).
          if (entry.phase === 'queued' || entry.phase === 'submitting') {
            entry.phase = 'starting';
          } else if (isTerminalPhase(entry.phase)) {
            entry.phase = 'starting';
          }
          break;
        default:
          break;
      }
      return next;
    }
    case 'stopped': {
      const entry = entryForOp(next, event.opId);
      if (entry !== null) {
        setPhase(entry, 'interrupted');
      }
      return next;
    }
    default:
      return next;
  }
}
