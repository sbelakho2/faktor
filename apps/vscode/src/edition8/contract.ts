// Edition 8 native read model (client side).
//
// There is deliberately NO aggregate `/native/ui/edition8` route: Revision 8
// screens read the daemon's real typed routes directly (health/ready, models,
// config snapshot, provider connections, model parameters, call receipts,
// context manifests, permissions policy/audit, MCP/tools, skills/hooks,
// sessions/meta/search, usage/outcomes, proof/verification/checkpoints,
// diagnostics/updater/identity/entitlements, browser, enterprise, workers).
// Each source is either a served document or a typed unavailable reason; a
// missing source never becomes a fabricated 0/empty/verified value.

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export type E8SourceState = 'ready' | 'error' | 'unavailable';

export interface E8Source {
  readonly state: E8SourceState;
  readonly method: 'GET' | 'POST';
  readonly path: string;
  readonly error: string | null;
  readonly observed_at_ms: number;
  readonly data: Json | null;
}

export type E8Sources = Readonly<Record<string, E8Source>>;

export interface E8BindingTuple {
  readonly session: string | null;
  readonly workspace: string | null;
  readonly worktree: string | null;
  readonly daemon: string | null;
  readonly generation: number | null;
}

export interface E8Snapshot {
  readonly schema: 'faktor-edition8-ui/v1';
  readonly revision: string;
  readonly generated_at_ms: number;
  readonly stale: boolean;
  readonly stale_age_ms: number | null;
  readonly binding: E8BindingTuple | null;
  readonly sources: E8Sources;
  readonly health: Json | null;
  readonly ready: Json | null;
  readonly models: Json | null;
  readonly capabilities: Json | null;
  readonly config: Json | null;
  readonly descriptors: Json | null;
  readonly connections: Json | null;
  readonly providers: Json | null;
  readonly parameters: Json | null;
  readonly presets: Json | null;
  readonly instructions: Json | null;
  readonly runtime: Json | null;
  readonly provenance: Json | null;
  readonly receipts: Json | null;
  readonly policy: Json | null;
  readonly permissionAudit: Json | null;
  readonly approvals: Json | null;
  readonly agents: Json | null;
  readonly board: Json | null;
  readonly tournaments: Json | null;
  readonly mcp: Json | null;
  readonly tools: Json | null;
  readonly skills: Json | null;
  readonly hooks: Json | null;
  readonly hookAudit: Json | null;
  readonly memory: Json | null;
  readonly sessions: Json | null;
  readonly sessionMeta: Json | null;
  readonly search: Json | null;
  readonly transcript: Json | null;
  readonly usage: Json | null;
  readonly sessionUsage: Json | null;
  readonly outcomes: Json | null;
  readonly proof: Json | null;
  readonly verification: Json | null;
  readonly checkpoints: Json | null;
  readonly taskRuns: Json | null;
  readonly tasks: Json | null;
  readonly turns: Json | null;
  readonly diagnostics: Json | null;
  readonly updater: Json | null;
  readonly identity: Json | null;
  readonly entitlements: Json | null;
  readonly browser: Json | null;
  readonly browserAdapters: Json | null;
  readonly browserSessions: Json | null;
  readonly contextPins: Json | null;
  readonly efficiency: Json | null;
  readonly budget: Json | null;
  readonly semanticCapabilities: Json | null;
  readonly parallelSessions: Json | null;
  readonly enterprise: Json | null;
  readonly workers: Json | null;
  readonly index: Json | null;
  readonly semantic: Json | null;
  readonly terminals: Json | null;
  readonly telemetry: Json | null;
}

export interface E8ConfigValue {
  readonly key: string;
  readonly value: Json;
  readonly scope: string;
  readonly source: string;
  readonly locked: boolean;
  readonly apply_state: string;
  readonly apply_timing: string;
}

export interface E8ConfigSnapshot {
  readonly revision: string;
  readonly runtime_revision: string;
  readonly values: readonly E8ConfigValue[];
  readonly secrets: readonly string[];
}

export interface E8ApplyResult {
  readonly ok: boolean;
  readonly message: string;
  readonly path: string;
  readonly body: Json | null;
  readonly error_code: string | null;
  readonly field_states: readonly { readonly key: string; readonly state: string }[];
}

// ------------------------------------------------------------ safe accessors

export function isObject(value: Json | null | undefined): value is { [key: string]: Json } {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

export function objectAt(value: Json | null | undefined, key: string): Json | null {
  if (!isObject(value)) return null;
  return value[key] ?? null;
}

export function arrayAt(value: Json | null | undefined, key: string): readonly Json[] {
  if (!isObject(value)) return [];
  const entry = value[key];
  return Array.isArray(entry) ? entry : [];
}

export function stringAt(value: Json | null | undefined, key: string): string | null {
  if (!isObject(value)) return null;
  const entry = value[key];
  return typeof entry === 'string' && entry.length > 0 ? entry : null;
}

export function numberAt(value: Json | null | undefined, key: string): number | null {
  if (!isObject(value)) return null;
  const entry = value[key];
  return typeof entry === 'number' && Number.isFinite(entry) ? entry : null;
}

export function booleanAt(value: Json | null | undefined, key: string): boolean | null {
  if (!isObject(value)) return null;
  const entry = value[key];
  return typeof entry === 'boolean' ? entry : null;
}

/** Locale grouping with exact value retained for copy/export (visual 3.1). */
export function exactNumber(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return 'Unavailable';
  return new Intl.NumberFormat('en-US', { maximumFractionDigits: 20, useGrouping: true }).format(value);
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

/** Money is micro-USD over the wire; render exact dollars, never round. */
export function moneyMicro(value: Json | null | undefined): string {
  if (typeof value === 'string' && /^-?\d+$/.test(value)) {
    return microString(value);
  }
  if (typeof value === 'number' && Number.isSafeInteger(value)) {
    return microString(String(value));
  }
  if (isObject(value) && typeof value.micro === 'string') return microString(value.micro);
  return 'Unavailable';
}

function microString(raw: string): string {
  const negative = raw.startsWith('-');
  const digits = (negative ? raw.slice(1) : raw).padStart(7, '0');
  const dollars = digits.slice(0, -6);
  const cents = digits.slice(-6).padStart(6, '0').slice(0, 2);
  return `${negative ? '-' : ''}$${new Intl.NumberFormat('en-US').format(Number(dollars))}.${cents}`;
}

export function boolText(value: boolean | null): string {
  return value === null ? 'Unavailable' : value ? 'Yes' : 'No';
}

export function stateText(value: string | null): string {
  return value === null ? 'Unavailable' : value;
}

export const E8_SOURCE_KEYS = [
  'health',
  'ready',
  'models',
  'capabilities',
  'config',
  'descriptors',
  'connections',
  'providers',
  'parameters',
  'presets',
  'instructions',
  'runtime',
  'provenance',
  'receipts',
  'policy',
  'permissionAudit',
  'approvals',
  'agents',
  'board',
  'tournaments',
  'mcp',
  'tools',
  'skills',
  'hooks',
  'hookAudit',
  'memory',
  'sessions',
  'sessionMeta',
  'search',
  'transcript',
  'usage',
  'sessionUsage',
  'outcomes',
  'proof',
  'verification',
  'checkpoints',
  'taskRuns',
  'tasks',
  'turns',
  'diagnostics',
  'updater',
  'identity',
  'entitlements',
  'browser',
  'browserAdapters',
  'browserSessions',
  'contextPins',
  'efficiency',
  'budget',
  'semanticCapabilities',
  'parallelSessions',
  'enterprise',
  'workers',
  'index',
  'semantic',
  'terminals',
  'telemetry',
] as const;

export type E8SourceKey = (typeof E8_SOURCE_KEYS)[number];
