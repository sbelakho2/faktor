// The refresh read plan. This is the executable product boundary for run
// refresh:
//
//   * an Ask conversation refresh reads conversation data only;
//   * a normal run refresh reads the run/task/proof projection;
//   * NEITHER plan reads billing, account, subscription or entitlement data.
//
// Commercial data is loaded lazily by the explicit Account/Usage surface.
// The tests (and their mutation witnesses) assert this plan, and the
// extension branches on the same plan.

export type RefreshRead =
  | 'conversation'
  | 'sessions'
  | 'messages'
  | 'permissions'
  | 'projection'
  | 'tasks'
  | 'verification'
  | 'sessionUsage'
  | 'agents'
  | 'taskRuns'
  | 'catalog'
  | 'board'
  | 'indexCoverage'
  | 'taskVerification'
  | 'proof'
  | 'tournament'
  | 'account';

export interface RefreshPlanInput {
  readonly askOnly: boolean;
}

const CONVERSATION_READS: readonly RefreshRead[] = [
  'conversation',
  'sessions',
  'messages',
  'permissions',
];

const RUN_READS: readonly RefreshRead[] = [
  'projection',
  'tasks',
  'verification',
  'sessionUsage',
  'agents',
  'taskRuns',
  'catalog',
  'sessions',
  'messages',
  'permissions',
  'board',
  'indexCoverage',
  'taskVerification',
  'proof',
  'tournament',
];

/** The exact read plan for one refresh. `account` is never automatic. */
export function refreshReadPlan(input: RefreshPlanInput): readonly RefreshRead[] {
  return input.askOnly ? CONVERSATION_READS : RUN_READS;
}

/** The explicit Account/Usage surface is the only billing reader. */
export function accountReadPlan(): readonly RefreshRead[] {
  return ['account'];
}
