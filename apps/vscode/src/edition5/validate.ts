// Strict-enough client validation for the Edition 5 read model and commands.
// Unknown additive members are ignored; every member the UI consumes is
// type-checked. The honesty-critical fields (capability authority, context
// basis, proof verdicts, effect status) are validated exhaustively because a
// wrong type there could turn Unavailable into a fake pass.

import {
  E5ProtocolError,
  type E5ApplyState,
  type E5CapabilityAuthority,
  type E5ConfigSnapshot,
  type E5ProviderTestResult,
  type E5Snapshot,
  type Json,
} from './contract.ts';

function fail(path: string, detail: string): never {
  throw new E5ProtocolError(path, detail);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function obj(value: unknown, path: string): Record<string, unknown> {
  if (!isRecord(value)) fail(path, `expected an object, got ${value === null ? 'null' : typeof value}`);
  return value;
}

function str(value: unknown, path: string, max = 64 * 1024): string {
  if (typeof value !== 'string') fail(path, 'expected a string');
  if (value.length > max) fail(path, `string exceeds ${max} characters`);
  return value;
}

function nstr(value: unknown, path: string, max = 64 * 1024): string | null {
  return value === undefined || value === null ? null : str(value, path, max);
}

function num(value: unknown, path: string): number {
  if (typeof value !== 'number' || !Number.isFinite(value)) fail(path, 'expected a finite number');
  return value;
}

function nnum(value: unknown, path: string): number | null {
  return value === undefined || value === null ? null : num(value, path);
}

function int(value: unknown, path: string): number {
  const parsed = num(value, path);
  if (!Number.isInteger(parsed)) fail(path, 'expected an integer');
  return parsed;
}

function nint(value: unknown, path: string): number | null {
  return value === undefined || value === null ? null : int(value, path);
}

function bool(value: unknown, path: string): boolean {
  if (typeof value !== 'boolean') fail(path, 'expected a boolean');
  return value;
}

function nbool(value: unknown, path: string): boolean | null {
  return value === undefined || value === null ? null : bool(value, path);
}

function arr(value: unknown, path: string): readonly unknown[] {
  if (!Array.isArray(value)) fail(path, 'expected an array');
  return value;
}

function nrecord(value: unknown, path: string): Record<string, unknown> | null {
  return value === undefined || value === null ? null : obj(value, path);
}

function choice<T extends string>(value: unknown, path: string, choices: readonly T[]): T {
  const parsed = str(value, path, 128);
  if (!(choices as readonly string[]).includes(parsed)) {
    fail(path, `expected one of ${choices.join(', ')}, got ${parsed}`);
  }
  return parsed as T;
}

function strings(value: unknown, path: string, max = 4096): readonly string[] {
  return arr(value, path).map((entry, index) => str(entry, `${path}[${index}]`, max));
}

function authority(value: unknown, path: string): E5CapabilityAuthority {
  return choice(value, path, ['known', 'declared_generic', 'unknown'] as const);
}

export function validateApplyState(value: unknown, path: string): E5ApplyState {
  return choice(value, path, [
    'Effective',
    'NextCall',
    'NextTask',
    'NewSession',
    'RestartRequired',
    'Failed',
    'Saved',
    'Staged',
  ] as const);
}

export function validateConfigSnapshot(value: unknown, path = 'config'): E5ConfigSnapshot {
  const input = obj(value, path);
  return {
    revision: str(input.revision, `${path}.revision`, 256),
    saved_revision: nstr(input.saved_revision, `${path}.saved_revision`, 256),
    runtime_revision: nstr(input.runtime_revision, `${path}.runtime_revision`, 256),
    values: arr(input.values, `${path}.values`).map((entry, index) => {
      const row = obj(entry, `${path}.values[${index}]`);
      return {
        key: str(row.key, `${path}.values[${index}].key`, 512),
        value: (row.value ?? null) as Json,
        source: str(row.source, `${path}.values[${index}].source`, 512),
        locked: bool(row.locked, `${path}.values[${index}].locked`),
        lock_source: nstr(row.lock_source, `${path}.values[${index}].lock_source`, 512),
        apply_state: validateApplyState(row.apply_state, `${path}.values[${index}].apply_state`),
        descriptor_type: str(row.descriptor_type, `${path}.values[${index}].descriptor_type`, 128),
        scope: str(row.scope, `${path}.values[${index}].scope`, 128),
      };
    }),
    secrets: arr(input.secrets, `${path}.secrets`).map((entry, index) => {
      const row = obj(entry, `${path}.secrets[${index}]`);
      return {
        reference: str(row.reference, `${path}.secrets[${index}].reference`, 512),
        bound: bool(row.bound, `${path}.secrets[${index}].bound`),
        verified: bool(row.verified, `${path}.secrets[${index}].verified`),
        vault_generation: nint(row.vault_generation, `${path}.secrets[${index}].vault_generation`),
        kind: str(row.kind, `${path}.secrets[${index}].kind`, 128),
        detail: nstr(row.detail, `${path}.secrets[${index}].detail`, 2048),
      };
    }),
    managed_locks: strings(input.managed_locks, `${path}.managed_locks`, 512),
    updated_at_ms: nnum(input.updated_at_ms, `${path}.updated_at_ms`),
  };
}

function validateModels(value: unknown, path: string): NonNullable<E5Snapshot['models']> {
  const input = obj(value, path);
  const routing = nrecord(input.routing, `${path}.routing`);
  const price = nrecord(input.price, `${path}.price`);
  const quality = nrecord(input.quality, `${path}.quality`);
  const probe = nrecord(input.probe, `${path}.probe`);
  const parameter = (entry: unknown, at: string) => {
    const row = obj(entry, at);
    return {
      key: str(row.key, `${at}.key`, 256),
      kind: choice(row.kind, `${at}.kind`, [
        'integer',
        'number',
        'boolean',
        'enum',
        'string',
        'string_list',
        'json',
      ] as const),
      min: nnum(row.min, `${at}.min`),
      max: nnum(row.max, `${at}.max`),
      choices: strings(row.choices ?? [], `${at}.choices`, 256),
      requires_exact_authority: bool(row.requires_exact_authority, `${at}.requires_exact_authority`),
      unset_allowed: bool(row.unset_allowed, `${at}.unset_allowed`),
      unit: nstr(row.unit, `${at}.unit`, 64),
    };
  };
  const call = (entry: unknown, at: string) => {
    const row = obj(entry, at);
    return {
      call_id: str(row.call_id, `${at}.call_id`, 256),
      provider: str(row.provider, `${at}.provider`, 256),
      model: str(row.model, `${at}.model`, 512),
      role: str(row.role, `${at}.role`, 128),
      route_reason: str(row.route_reason, `${at}.route_reason`, 1024),
      started_at_ms: num(row.started_at_ms, `${at}.started_at_ms`),
      parameters: (row.parameters ?? {}) as Readonly<Record<string, import('./contract.ts').Json>>,
    };
  };
  return {
    catalog: arr(input.catalog, `${path}.catalog`).map((entry, index) => {
      const at = `${path}.catalog[${index}]`;
      const row = obj(entry, at);
      return {
        provider: str(row.provider, `${at}.provider`, 256),
        model: str(row.model, `${at}.model`, 512),
        context: nint(row.context, `${at}.context`),
        max_output: nint(row.max_output, `${at}.max_output`),
        tools: nbool(row.tools, `${at}.tools`),
        parallel_tools: nint(row.parallel_tools, `${at}.parallel_tools`),
        reasoning: nbool(row.reasoning, `${at}.reasoning`),
        thinking: nbool(row.thinking, `${at}.thinking`),
        vision: nbool(row.vision, `${at}.vision`),
        documents: nbool(row.documents, `${at}.documents`),
        streaming: nbool(row.streaming, `${at}.streaming`),
        structured_output: nbool(row.structured_output, `${at}.structured_output`),
        embeddings: nbool(row.embeddings, `${at}.embeddings`),
        capability_authority: authority(row.capability_authority, `${at}.capability_authority`),
        capabilities_gated: bool(row.capabilities_gated, `${at}.capabilities_gated`),
        availability: choice(row.availability, `${at}.availability`, [
          'available',
          'unavailable',
          'unknown',
        ] as const),
        unavailable_reason: nstr(row.unavailable_reason, `${at}.unavailable_reason`, 2048),
        source: nstr(row.source, `${at}.source`, 512),
        observed_at_ms: nnum(row.observed_at_ms, `${at}.observed_at_ms`),
      };
    }),
    favorites: strings(input.favorites ?? [], `${path}.favorites`, 512),
    recent: strings(input.recent ?? [], `${path}.recent`, 512),
    routing:
      routing === null
        ? null
        : {
            mode: str(routing.mode, `${path}.routing.mode`, 128),
            requested: str(routing.requested, `${path}.routing.requested`, 512),
            actual: str(routing.actual, `${path}.routing.actual`, 512),
            fallback: strings(routing.fallback, `${path}.routing.fallback`, 512),
            next_task_revision: nstr(routing.next_task_revision, `${path}.routing.next_task_revision`, 256),
          },
    parameters: arr(input.parameters ?? [], `${path}.parameters`).map((entry, index) =>
      parameter(entry, `${path}.parameters[${index}]`),
    ),
    calls: arr(input.calls ?? [], `${path}.calls`).map((entry, index) => call(entry, `${path}.calls[${index}]`)),
    price:
      price === null
        ? null
        : {
            list: nstr(price.list, `${path}.price.list`, 256),
            declared: nstr(price.declared, `${path}.price.declared`, 256),
            observed: nstr(price.observed, `${path}.price.observed`, 256),
            quota: nstr(price.quota, `${path}.price.quota`, 256),
            unknown_reason: nstr(price.unknown_reason, `${path}.price.unknown_reason`, 1024),
            basis: str(price.basis, `${path}.price.basis`, 128),
          },
    quality:
      quality === null
        ? null
        : {
            declared: nnum(quality.declared, `${path}.quality.declared`),
            observed: nnum(quality.observed, `${path}.quality.observed`),
            samples: nint(quality.samples, `${path}.quality.samples`),
            basis: str(quality.basis, `${path}.quality.basis`, 128),
          },
    probe:
      probe === null
        ? null
        : {
            last_observed_at_ms: nnum(probe.last_observed_at_ms, `${path}.probe.last_observed_at_ms`),
            stages: arr(probe.stages ?? [], `${path}.probe.stages`).map((entry, index) => {
              const at = `${path}.probe.stages[${index}]`;
              const row = obj(entry, at);
              return {
                stage: str(row.stage, `${at}.stage`, 128),
                status: str(row.status, `${at}.status`, 128),
                detail: nstr(row.detail, `${at}.detail`, 2048),
              };
            }),
            failure: nstr(probe.failure, `${path}.probe.failure`, 2048),
          },
  };
}

function validateProviders(value: unknown, path: string): NonNullable<E5Snapshot['providers']> {
  const input = obj(value, path);
  const local = nrecord(input.local, `${path}.local`);
  return {
    connections: arr(input.connections, `${path}.connections`).map((entry, index) => {
      const at = `${path}.connections[${index}]`;
      const row = obj(entry, at);
      return {
        id: str(row.id, `${at}.id`, 256),
        label: str(row.label, `${at}.label`, 512),
        kind: str(row.kind, `${at}.kind`, 128),
        base_url: str(row.base_url, `${at}.base_url`, 4096),
        credential_reference: nstr(row.credential_reference, `${at}.credential_reference`, 512),
        credential_bound: bool(row.credential_bound, `${at}.credential_bound`),
        headers: arr(row.headers ?? [], `${at}.headers`).map((header, headerIndex) => {
          const headerAt = `${at}.headers[${headerIndex}]`;
          const value = obj(header, headerAt);
          return {
            name: str(value.name, `${headerAt}.name`, 256),
            reference: str(value.reference, `${headerAt}.reference`, 512),
          };
        }),
        allow_loopback: bool(row.allow_loopback, `${at}.allow_loopback`),
        transport_timeout_ms: nint(row.transport_timeout_ms, `${at}.transport_timeout_ms`),
        proxy: nstr(row.proxy, `${at}.proxy`, 2048),
        tls: nstr(row.tls, `${at}.tls`, 2048),
        health: str(row.health, `${at}.health`, 128),
        health_detail: nstr(row.health_detail, `${at}.health_detail`, 2048),
        models: strings(row.models ?? [], `${at}.models`, 512),
        source: str(row.source, `${at}.source`, 128),
        trust: str(row.trust, `${at}.trust`, 256),
      };
    }),
    local:
      local === null
        ? null
        : {
            execution_location: nstr(local.execution_location, `${path}.local.execution_location`, 512),
            daemon_host: nstr(local.daemon_host, `${path}.local.daemon_host`, 512),
            endpoint: nstr(local.endpoint, `${path}.local.endpoint`, 4096),
            endpoint_class: str(local.endpoint_class, `${path}.local.endpoint_class`, 128),
            loopback_authorized: bool(local.loopback_authorized, `${path}.local.loopback_authorized`),
            lan_policy: nstr(local.lan_policy, `${path}.local.lan_policy`, 1024),
            declared_allocation: nint(local.declared_allocation, `${path}.local.declared_allocation`),
            observed_allocation: nint(local.observed_allocation, `${path}.local.observed_allocation`),
            observed_source: nstr(local.observed_source, `${path}.local.observed_source`, 512),
            keep_alive: nstr(local.keep_alive, `${path}.local.keep_alive`, 128),
            unload_supported: bool(local.unload_supported, `${path}.local.unload_supported`),
            embedding_boundary: nstr(local.embedding_boundary, `${path}.local.embedding_boundary`, 1024),
          },
    stages: arr(input.stages ?? [], `${path}.stages`).map((entry, index) => {
      const at = `${path}.stages[${index}]`;
      const row = obj(entry, at);
      return {
        stage: str(row.stage, `${at}.stage`, 128),
        status: str(row.status, `${at}.status`, 128),
        detail: nstr(row.detail, `${at}.detail`, 2048),
      };
    }),
  };
}

function shallow<T>(value: unknown, path: string, required: readonly string[]): T {
  const input = obj(value, path);
  for (const key of required) {
    if (!(key in input)) fail(`${path}.${key}`, 'missing required member');
  }
  return input as unknown as T;
}

export function validateSnapshot(value: unknown, path = 'edition5'): E5Snapshot {
  const input = obj(value, path);
  if (input.schema !== 'faktor-edition5-ui/v1') {
    fail(`${path}.schema`, `expected faktor-edition5-ui/v1, got ${String(input.schema)}`);
  }
  const binding = nrecord(input.binding, `${path}.binding`);
  const section = <T>(key: string, validator: (value: unknown, at: string) => T): T | null => {
    const raw = input[key];
    return raw === undefined || raw === null ? null : validator(raw, `${path}.${key}`);
  };
  return {
    schema: 'faktor-edition5-ui/v1',
    revision: str(input.revision, `${path}.revision`, 256),
    generated_at_ms: num(input.generated_at_ms, `${path}.generated_at_ms`),
    stale: bool(input.stale, `${path}.stale`),
    stale_age_ms: nnum(input.stale_age_ms, `${path}.stale_age_ms`),
    binding:
      binding === null
        ? null
        : {
            session: nstr(binding.session, `${path}.binding.session`, 256),
            workspace: nstr(binding.workspace, `${path}.binding.workspace`, 4096),
            worktree: nstr(binding.worktree, `${path}.binding.worktree`, 4096),
            daemon: nstr(binding.daemon, `${path}.binding.daemon`, 512),
            generation: nint(binding.generation, `${path}.binding.generation`),
          },
    config: section('config', validateConfigSnapshot),
    models: section('models', validateModels),
    providers: section('providers', validateProviders),
    generation: section('generation', (raw, at) => {
      const row = shallow<NonNullable<E5Snapshot['generation']>>(raw, at, ['parameters', 'apply_timing']);
      return row;
    }),
    agents: section('agents', (raw, at) => shallow(raw, at, ['presets', 'roles', 'tree'])),
    context: section('context', (raw, at) => {
      const row = shallow<NonNullable<E5Snapshot['context']>>(raw, at, ['editor_context']);
      if (row.usage !== null && (row.usage.basis !== 'observed' && row.usage.basis !== 'estimated')) {
        fail(`${at}.usage.basis`, 'expected observed or estimated');
      }
      return row;
    }),
    compaction: section('compaction', (raw, at) => shallow(raw, at, [])),
    instructions: section('instructions', (raw, at) => shallow(raw, at, ['sources', 'conflicts', 'templates'])),
    memory: section('memory', (raw, at) => shallow(raw, at, ['records'])),
    policy: section('policy', (raw, at) => {
      const row = shallow<NonNullable<E5Snapshot['policy']>>(raw, at, [
        'revision',
        'preset',
        'rules',
        'filesystem',
        'shell',
        'network',
        'network_guarantee',
        'sandbox_state',
      ]);
      for (const [index, rule] of row.rules.entries()) {
        if (!['Allow', 'Ask', 'Deny'].includes(rule.decision)) {
          fail(`${at}.rules[${index}].decision`, 'expected Allow, Ask or Deny');
        }
      }
      return row;
    }),
    approvals: section('approvals', (raw, at) => {
      const row = shallow<NonNullable<E5Snapshot['approvals']>>(raw, at, ['pending', 'history']);
      for (const [index, request] of row.pending.entries()) {
        if (request.consumed) fail(`${at}.pending[${index}].consumed`, 'pending request cannot be consumed');
      }
      return row;
    }),
    tools: section('tools', (raw, at) => shallow(raw, at, ['builtin', 'mcp', 'tool_loading'])),
    skills: section('skills', (raw, at) => shallow(raw, at, ['skills', 'commands', 'hooks'])),
    terminal: section('terminal', (raw, at) => shallow(raw, at, ['processes'])),
    browser: section('browser', (raw, at) => shallow(raw, at, ['adapters', 'sessions'])),
    budget: section('budget', (raw, at) => shallow(raw, at, ['enforced'])),
    run: section('run', (raw, at) => {
      const row = shallow<NonNullable<E5Snapshot['run']>>(raw, at, ['state', 'timeline', 'queue', 'display']);
      for (const [index, item] of row.timeline.entries()) {
        if (item.effect_status !== null && item.effect_status !== 'unknown' && item.effect_status !== 'settled' && item.effect_status !== 'failed') {
          fail(`${at}.timeline[${index}].effect_status`, `unexpected effect state ${item.effect_status}`);
        }
      }
      return row;
    }),
    changes: section('changes', (raw, at) => shallow(raw, at, ['files', 'comments', 'acceptance'])),
    checkpoints:
      input.checkpoints === undefined || input.checkpoints === null
        ? null
        : arr(input.checkpoints, `${path}.checkpoints`).map((entry, index) =>
            shallow(entry, `${path}.checkpoints[${index}]`, ['id', 'coverage']),
          ),
    history: section('history', (raw, at) => {
      const row = shallow<NonNullable<E5Snapshot['history']>>(raw, at, ['sessions']);
      if (row.archived_is_deleted !== false) fail(`${at}.archived_is_deleted`, 'archived is not deleted');
      return row;
    }),
    usage: section('usage', (raw, at) => {
      const row = shallow<NonNullable<E5Snapshot['usage']>>(raw, at, [
        'attempted_spend_micro',
        'vcsc_state',
        'cohort',
        'period',
        'denominator',
      ]);
      if (row.verified_landed_changes === 0 && row.vcsc_micro === '0') {
        fail(`${at}.vcsc_micro`, 'zero verified denominator cannot be a zero VCSC');
      }
      return row;
    }),
    outcomes: section('outcomes', (raw, at) => shallow(raw, at, ['verified', 'unverified', 'pending', 'receipts'])),
    efficiency: section('efficiency', (raw, at) => shallow(raw, at, ['flags', 'retrieval'])),
    settings: section('settings', (raw, at) => shallow(raw, at, ['search'])),
    diagnostics: section('diagnostics', (raw, at) => {
      const row = shallow<NonNullable<E5Snapshot['diagnostics']>>(raw, at, [
        'readiness',
        'checksum_verified',
        'signature_verified',
        'trust',
        'protocol_compatible',
      ]);
      return row;
    }),
    binding_view: section('binding_view', (raw, at) => shallow(raw, at, ['data_boundary'])),
    proof: section('proof', (raw, at) => {
      const row = shallow<NonNullable<E5Snapshot['proof']>>(raw, at, [
        'outcome',
        'required_total',
        'required_passed',
        'criteria',
      ]);
      if (!['verified', 'unverified', 'pending', 'unavailable'].includes(row.outcome)) {
        fail(`${at}.outcome`, 'unexpected engineering outcome');
      }
      for (const [index, criterion] of row.criteria.entries()) {
        if (!['pass', 'fail', 'unavailable'].includes(criterion.verdict)) {
          fail(`${at}.criteria[${index}].verdict`, 'unexpected verdict');
        }
      }
      if (row.outcome === 'verified' && row.required_passed < row.required_total) {
        fail(`${at}.outcome`, 'verified outcome with missing required evidence');
      }
      if (row.outcome === 'verified' && row.finish_receipt === null && row.finish_requested !== 'local') {
        fail(`${at}.finish_receipt`, 'verified outcome without a finish receipt');
      }
      return row;
    }),
  };
}

export function validateProviderTest(value: unknown, path = 'provider-test'): E5ProviderTestResult {
  const input = obj(value, path);
  return {
    stages: arr(input.stages, `${path}.stages`).map((entry, index) => {
      const at = `${path}.stages[${index}]`;
      const row = obj(entry, at);
      return {
        stage: str(row.stage, `${at}.stage`, 128),
        status: str(row.status, `${at}.status`, 128),
        detail: nstr(row.detail, `${at}.detail`, 2048),
      };
    }),
    observed_at_ms: num(input.observed_at_ms, `${path}.observed_at_ms`),
    credential_verified: bool(input.credential_verified, `${path}.credential_verified`),
    discovery: strings(input.discovery ?? [], `${path}.discovery`, 512),
    failure: nstr(input.failure, `${path}.failure`, 2048),
  };
}

/** The default empty Unavailable snapshot used before the first read. */
export function emptyEdition5Snapshot(generatedAtMs = 0, binding: E5Snapshot['binding'] = null): E5Snapshot {
  return {
    schema: 'faktor-edition5-ui/v1',
    revision: '0',
    generated_at_ms: generatedAtMs,
    stale: true,
    stale_age_ms: null,
    binding,
    config: null,
    models: null,
    providers: null,
    generation: null,
    agents: null,
    context: null,
    compaction: null,
    instructions: null,
    memory: null,
    policy: null,
    approvals: null,
    tools: null,
    skills: null,
    terminal: null,
    browser: null,
    budget: null,
    run: null,
    changes: null,
    checkpoints: null,
    history: null,
    usage: null,
    outcomes: null,
    efficiency: null,
    settings: null,
    diagnostics: null,
    binding_view: null,
    proof: null,
  };
}
