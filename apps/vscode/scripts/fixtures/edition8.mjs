// Test-only Revision 8 fixtures for the registry-driven harnesses.
//
// These are NOT product data and never ship in an activated panel: every
// fixture is passed only to the harness renderer. Fixture values are shaped
// exactly like the real served routes (camelCase/snake_case both exercised)
// so the view model's empty/stale/error/disabled paths are tested honestly.

const NOW = 1_760_000_000_000;

function source(method, path, data) {
  return { state: 'ready', method, path, error: null, observed_at_ms: NOW, data };
}

const config = {
  revision: 7,
  runtime_revision: 7,
  values: {
    'faktor.defaultProvider': { key: 'faktor.defaultProvider', value: 'openai', scope: 'user', source: 'user', locked: false, apply_timing: 'next_call', apply_state: 'NextCall' },
    'faktor.defaultModel': { key: 'faktor.defaultModel', value: 'gpt-5-codex', scope: 'user', source: 'user', locked: false, apply_timing: 'next_call', apply_state: 'NextCall' },
    'faktor.model.contextLimit': { key: 'faktor.model.contextLimit', value: 200000, scope: 'user', source: 'default', locked: false, apply_timing: 'next_task', apply_state: 'NextTask' },
    'faktor.generation.maxOutput': { key: 'faktor.generation.maxOutput', value: 16384, scope: 'user', source: 'default', locked: false, apply_timing: 'next_call', apply_state: 'NextCall' },
    'faktor.generation.temperature': { key: 'faktor.generation.temperature', value: 0.2, scope: 'user', source: 'user', locked: false, apply_timing: 'next_call', apply_state: 'NextCall' },
    'faktor.generation.topP': { key: 'faktor.generation.topP', value: 0.95, scope: 'user', source: 'user', locked: false, apply_timing: 'next_call', apply_state: 'NextCall' },
    'faktor.context.compactionMinReduction': { key: 'faktor.context.compactionMinReduction', value: 0.3, scope: 'user', source: 'default', locked: false, apply_timing: 'next_task', apply_state: 'NextTask' },
    'faktor.efficiency.failureLearning': { key: 'faktor.efficiency.failureLearning', value: true, scope: 'user', source: 'default', locked: false, apply_timing: 'new_session', apply_state: 'NewSession' },
    'faktor.efficiency.ccr': { key: 'faktor.efficiency.ccr', value: true, scope: 'user', source: 'default', locked: false, apply_timing: 'new_session', apply_state: 'NewSession' },
    'faktor.efficiency.typedHandoff': { key: 'faktor.efficiency.typedHandoff', value: true, scope: 'user', source: 'default', locked: false, apply_timing: 'new_session', apply_state: 'NewSession' },
    'faktor.efficiency.semanticContext': { key: 'faktor.efficiency.semanticContext', value: true, scope: 'user', source: 'default', locked: false, apply_timing: 'new_session', apply_state: 'NewSession' },
    'faktor.efficiency.reworkGuard': { key: 'faktor.efficiency.reworkGuard', value: true, scope: 'user', source: 'default', locked: false, apply_timing: 'new_session', apply_state: 'NewSession' },
    'faktor.memory.captureMode': { key: 'faktor.memory.captureMode', value: 'manual', scope: 'user', source: 'default', locked: false, apply_timing: 'next_call', apply_state: 'NextCall' },
    'faktor.permissions.preset': { key: 'faktor.permissions.preset', value: 'ask', scope: 'user', source: 'default', locked: false, apply_timing: 'next_task', apply_state: 'NextTask' },
    'faktor.permissions.allowedTools': { key: 'faktor.permissions.allowedTools', value: ['read_file', 'list_directory'], scope: 'user', source: 'user', locked: false, apply_timing: 'next_task', apply_state: 'NextTask' },
    'faktor.sandbox.shell': { key: 'faktor.sandbox.shell', value: 'ask', scope: 'user', source: 'default', locked: false, apply_timing: 'next_task', apply_state: 'NextTask' },
    'faktor.sandbox.network': { key: 'faktor.sandbox.network', value: 'deny', scope: 'user', source: 'default', locked: false, apply_timing: 'next_task', apply_state: 'NextTask' },
    'faktor.sandbox.allowLoopback': { key: 'faktor.sandbox.allowLoopback', value: false, scope: 'user', source: 'default', locked: false, apply_timing: 'new_session', apply_state: 'NewSession' },
    'faktor.budget.maxTokens': { key: 'faktor.budget.maxTokens', value: 0, scope: 'user', source: 'default', locked: false, apply_timing: 'next_task', apply_state: 'NextTask' },
    'faktor.budget.maxCostMicro': { key: 'faktor.budget.maxCostMicro', value: '5000000', scope: 'user', source: 'user', locked: false, apply_timing: 'next_task', apply_state: 'NextTask' },
    'faktor.budget.warnAtPercent': { key: 'faktor.budget.warnAtPercent', value: 80, scope: 'user', source: 'default', locked: false, apply_timing: 'next_task', apply_state: 'NextTask' },
    'faktor.retry.maxAttempts': { key: 'faktor.retry.maxAttempts', value: 3, scope: 'user', source: 'default', locked: false, apply_timing: 'next_call', apply_state: 'NextCall' },
    'faktor.retry.baseDelayMs': { key: 'faktor.retry.baseDelayMs', value: 250, scope: 'user', source: 'default', locked: false, apply_timing: 'next_call', apply_state: 'NextCall' },
    'faktor.runtime.binaryPath': { key: 'faktor.runtime.binaryPath', value: '/usr/local/bin/faktor', scope: 'machine', source: 'machine', locked: true, apply_timing: 'restart_required', apply_state: 'RestartRequired' },
    'faktor.telemetry.enabled': { key: 'faktor.telemetry.enabled', value: false, scope: 'user', source: 'default', locked: false, apply_timing: 'next_call', apply_state: 'NextCall' },
    'faktor.telemetry.categories': { key: 'faktor.telemetry.categories', value: [], scope: 'user', source: 'default', locked: false, apply_timing: 'next_call', apply_state: 'NextCall' },
    'faktor.retrieval.strategy': { key: 'faktor.retrieval.strategy', value: 'hybrid', scope: 'user', source: 'default', locked: false, apply_timing: 'new_session', apply_state: 'NewSession' },
    'faktor.retrieval.embeddingProvider': { key: 'faktor.retrieval.embeddingProvider', value: 'local', scope: 'user', source: 'default', locked: false, apply_timing: 'new_session', apply_state: 'NewSession' },
  },
};

const sources = {
  health: source('GET', '/native/health', { ok: true, version: '1.8.0' }),
  ready: source('GET', '/native/ready', { ready: true }),
  models: source('GET', '/models', [
    { provider: 'openai', model: 'gpt-5-codex', availability: 'available', capabilityAuthority: 'known' },
    { provider: 'anthropic', model: 'claude-sonnet-4-6', availability: 'available', capabilityAuthority: 'known' },
    { provider: 'local', model: 'qwen3-coder', availability: 'unavailable', capabilityAuthority: 'declared_generic' },
  ]),
  capabilities: source('GET', '/capabilities', { provider: 'openai', streaming: true, toolCalls: true }),
  config: source('GET', '/native/config/snapshot', config),
  descriptors: source('GET', '/native/config/descriptors', { descriptors: [{ key: 'faktor.defaultModel', category: 'models' }] }),
  connections: source('GET', '/native/providers/connections', {
    connections: [
      { id: 'openai-main', label: 'OpenAI', family: 'openai_chat', endpoint: 'https://api.openai.com/v1', credential_ref: 'env:OPENAI_API_KEY', apply_state: 'effective' },
      { id: 'ollama-local', label: 'Ollama', family: 'ollama', endpoint: 'http://127.0.0.1:11434', credential_ref: null, apply_state: 'effective' },
    ],
  }),
  providers: source('GET', '/native/providers', { local: 'ollama', providers: [{ id: 'openai-main', endpoint: 'https://api.openai.com/v1' }] }),
  presets: source('GET', '/native/presets', {
    presets: [
      { id: 'preset-code', mode: 'code', name: 'Code', version: 3, locked: false, modelProvider: 'openai', modelId: 'gpt-5-codex', roleInstructions: 'Implement with proof.', toolAllow: ['read_file'], childConcurrency: 2, budgetSharePpm: 500000 },
      { id: 'preset-ask', mode: 'ask', name: 'Ask', version: 1, locked: false, modelProvider: 'anthropic', modelId: 'claude-sonnet-4-6', roleInstructions: 'Answer without mutating.', toolAllow: [], childConcurrency: 0, budgetSharePpm: 500000 },
    ],
  }),
  instructions: source('GET', '/native/instructions/records', {
    records: [
      { id: 'instr-1', kind: 'instruction', title: 'User style', origin: 'user', scope: 'user', activation: 'always', enabled: true, locked: false, version: 2, content: 'Be concise.' },
      { id: 'rule-1', kind: 'rule', title: 'Project test rule', origin: 'project', scope: 'project', activation: 'glob', globs: ['**/*.rs'], enabled: true, locked: false, version: 1, content: 'Run cargo test.' },
      { id: 'style-1', kind: 'output_style', title: 'Output style', origin: 'user', scope: 'user', activation: 'always', enabled: true, locked: false, version: 1, content: 'Terse.' },
    ],
  }),
  runtime: source('GET', '/native/runtime/status', {
    version: '1.8.0',
    protocol: 'v1',
    executablePath: '/usr/local/bin/faktor',
    dataDir: '/tmp/faktor',
    trust: { state: 'unconfirmed', executablePath: '/usr/local/bin/faktor', checksum: 'sha256:runtime', publisherSignature: 'not verified' },
    shellIsolation: { mode: 'ask', networkGuarantee: 'deny', filesystemGuarantee: 'workspace_write' },
  }),
  provenance: source('GET', '/native/config/provenance', {
    revision: 7,
    runtimeRevision: 7,
    rows: [
      { key: 'faktor.retry.maxAttempts', label: 'Total attempts', category: 'budgets', applyTiming: 'next_call', effective: { value: 3, scope: 'project', source: 'project' } },
      { key: 'faktor.context.compactionMinReduction', label: 'Minimum reduction', category: 'context', applyTiming: 'next_task', effective: { value: 0.3, scope: 'user', source: 'user' } },
    ],
    conflicts: [{ key: 'faktor.permissions.preset', reason: 'project overrides user' }],
  }),
  parameters: source('GET', '/native/model-parameters', {
    provider: 'openai',
    model: 'gpt-5-codex',
    authority: 'known',
    apply_timing: 'next_call',
    parameters: [
      { key: 'temperature', kind: 'number', min: 0, max: 2 },
      { key: 'top_p', kind: 'number', min: 0, max: 1 },
      { key: 'reasoning', kind: 'enum', values: ['low', 'medium', 'high'] },
    ],
  }),
  receipts: source('GET', '/native/call-receipts', { receipts: [{ id: 'r-1', model: 'openai/gpt-5-codex', state: 'completed' }] }),
  policy: source('GET', '/native/permissions/policy', { revision: 4, preset: 'ask', rules: [], sandbox_state: { state: 'ask' } }),
  permissionAudit: source('GET', '/native/permissions/audit', { facts: [{ id: 'f-1', decision: 'ask' }] }),
  approvals: source('GET', '/native/permissions', { permissions: [{ id: 'perm-1', session_id: 's1', capability: 'shell.exec', detail: 'npm test' }] }),
  mcp: source('GET', '/native/mcp/servers', {
    servers: [
      {
        record: { id: 'mcp-1', name: 'filesystem', command: 'npx', args: ['-y', '@modelcontextprotocol/server-filesystem'], env: [] },
        health: { state: 'ready', note: 'initialize handshake completed' },
        tools: [{ name: 'read_file' }],
        remoteTransport: { supported: false, reason: 'stdio only' },
        logs: { available: false, reason: 'no retained log buffer' },
      },
    ],
  }),
  tools: source('GET', '/native/tools', { tools: [{ name: 'read_file', namespace: 'builtin', description: 'Read a file', health: 'ready' }, { name: 'mcp:filesystem:read_file', namespace: 'mcp:filesystem', description: 'Read via MCP', health: 'ready' }] }),
  skills: source('GET', '/native/skills', { skills: [{ id: 'skill-1', name: 'release', scope: 'project', invocation: 'manual' }] }),
  hooks: source('GET', '/native/hooks', { hooks: [{ record: { id: 'hook-1', event: 'pre_tool', command: './hook.sh', args: [] }, registered: true }], events: ['pre_tool', 'post_tool'], registryWired: true }),
  hookAudit: source('GET', '/native/hooks/audit', { facts: [{ id: 'hf-1', state: 'pass' }] }),
  memory: source('GET', '/native/memory/records', { records: [{ id: 'mem-1', kind: 'project', key: 'build-command', scope: 'project', source: 'manual', version: 2, text: 'Run pnpm build before tests.', deleted: false }] }),
  sessions: source('GET', '/native/sessions', { sessions: [{ id: 's1', title: 'Rev8 work', state: 'running', model: 'gpt-5-codex', active: true, updatedMs: NOW }, { id: 's2', title: 'Earlier', state: 'idle', model: 'claude-sonnet-4-6', updatedMs: NOW - 60000 }] }),
  sessionMeta: source('GET', '/native/sessions/s1/meta', { pinned: false, archived: false, updated_ms: NOW }),
  search: source('GET', '/native/transcript/search', { results: [{ role: 'assistant', snippet: 'the daemon contract is authoritative' }] }),
  transcript: source('GET', '/native/session/s1/turns', { turns: [{ role: 'user', text: 'Implement Rev8' }, { role: 'assistant', text: 'Reading the spec.' }] }),
  usage: source('GET', '/native/usage', { attemptedSpendMicro: '1234567', local: { tokens: 30000 }, hosted: { tokens: 12000 }, run: { state: 'running' } }),
  sessionUsage: source('GET', '/native/session/s1/usage', { tokens: 12000, costMicro: '450000', context: { basis: 'observed', capacity: 200000, manifest: 'ctx-7' }, terminals: [{ id: 'term-1' }] }),
  outcomes: source('GET', '/native/outcomes', { verifiedLandedChanges: 4, vcscMicro: '312500', failedAttempts: 1, verified: [{ id: 'o1' }], unverified: [], pending: [], receipts: [{ id: 'or1' }] }),
  verification: source('GET', '/native/session/s1/verification', { state: 'verified', candidate: { id: 'cand-1', digest: 'sha256:aaa' }, candidateDigest: 'sha256:aaa', criteria: { total: 3 }, diff: { base: 'HEAD', candidate: 'candidate-1' } }),
  checkpoints: source('GET', '/native/session/s1/checkpoints', { checkpoints: [{ id: 'cp-1', sequence: 1, coverage: 'full', created_at: '2026-10-18T00:00:00Z' }] }),
  taskRuns: source('GET', '/native/session/s1/task-runs', { run: { state: 'running' }, queued: [{ text: 'next message', state: 'queued' }], timeline: [{ kind: 'tool', state: 'running', detail: 'read_file' }], completion: { level: 'local', contract: 'criteria' } }),
  tasks: source('GET', '/native/session/s1/tasks', { tasks: [{ id: 'task-1', title: 'Implement Rev8' }] }),
  turns: source('GET', '/native/session/s1/turns', { turns: [{ role: 'user', text: 'Implement Rev8' }] }),
  diagnostics: source('GET', '/native/diagnostics/preview', { runtime: { checksum: 'sha256:runtime', signature: 'not recorded', trust: 'machine' }, protocol: { status: 'compatible', expected: '1', received: '1' } }),
  updater: source('GET', '/native/updater/status', { state: 'idle' }),
  identity: source('GET', '/native/identity', { user: 'dev', organization: 'local', status: 'connected' }),
  entitlements: source('GET', '/native/entitlements', { quota: 'unlimited', status: 'active' }),
  browser: source('GET', '/native/browser/status', { available: true, state: 'ready', url: 'http://127.0.0.1:3000' }),
  enterprise: source('GET', '/native/enterprise/status', { state: 'disabled', effectiveConfig: { locked: false } }),
  workers: source('GET', '/native/workers', { workers: [{ id: 'w1', state: 'ready' }] }),
  index: source('GET', '/native/index/coverage', { coverage: 0.98, stale: false }),
  semantic: source('GET', '/native/semantic/status', { state: 'ready', children: [] }),
  terminals: source('GET', '/native/terminals', { terminals: [{ id: 'term-1', name: 'dev', state: 'running', cwd: '/repo', owner: 's1' }] }),
  browserAdapters: source('GET', '/native/browser/adapters', { adapters: [{ id: 'chromium', mode: 'local', networkScope: 'loopback_only', advertised: true }], advertised: true, state: 'advertised' }),
  browserSessions: source('GET', '/native/browser/sessions', { sessions: [{ id: 'bs-1', state: 'requested', url: 'http://127.0.0.1:3000' }] }),
  contextPins: source('GET', '/native/session/s1/context/pins', { pins: [{ sourceId: 'src-1', action: 'pin' }] }),
  efficiency: source('GET', '/native/session/s1/efficiency', { state: 'observed', compaction: [], cache: { state: 'unavailable' }, routing: { recordedReceipts: 1 }, tokens: { state: 'observed', input: 9000, output: 3000 } }),
  budget: source('GET', '/native/session/s1/budget', { state: 'known', maxCostMicro: '5000000', spentCostMicro: '450000', openReservedMicro: '0', openReservations: 0, uncertainReservedMicro: '0', uncertainReservations: 0, settledReservations: 2, freeMicro: '4550000' }),
  semanticCapabilities: source('GET', '/native/semantic/capabilities', { providers: [], state: 'ready' }),
  parallelSessions: source('GET', '/native/sessions/parallel', { sessions: [{ id: 's1', state: 'running', active: true }, { id: 's2', state: 'idle', active: false }], activeRuns: 1 }),
  agents: source('GET', '/native/session/s1/agents', { agents: [{ id: 'a1', role: 'implementer', state: 'running', model: 'openai/gpt-5-codex', parent_id: null, worktree: 'wt1' }] }),
  board: source('GET', '/native/session/s1/board', { posts: [{ id: '1', author: 'agent', text: 'Reviewing the control ledger.' }] }),
  tournaments: source('GET', '/native/session/s1/tournaments', { tournaments: [{ id: 't1', state: 'decided', winner: 'openai/gpt-5-codex' }] }),
  proof: source('GET', '/native/tasks/task-1/proof', {
    proofState: 'verified',
    gate: { status: 'pass' },
    criteria: [
      { id: 'AC-01', title: 'Controls wired', verdict: 'pass' },
      { id: 'AC-02', title: 'No fake states', verdict: 'pass' },
      { id: 'AC-03', title: 'A11y', verdict: 'pass' },
    ],
    requiredChecks: { total: 3, passed: 3 },
    review: { status: 'reviewed' },
    integration: { state: 'integrated', remote: 'origin' },
    publication: { state: 'published', receipt: 'commit:abc' },
    completionSteps: [{ id: 'step-1', status: 'pass', detail: 'controls' }, { id: 'step-2', status: 'pass', detail: 'verification' }],
    evidence: { id: 'ev-1', excerpt: 'test output' },
    verifiedDigest: 'sha256:aaa',
    landed: { digest: 'sha256:aaa', landedEqualsVerified: true },
    cost: { state: 'folded' },
    contract: 'criteria',
    verified: { digest: 'sha256:aaa' },
  }),
};

export function edition8Snapshot(overrides = {}) {
  const base = {
    schema: 'faktor-edition8-ui/v1',
    revision: '7',
    generated_at_ms: NOW,
    stale: false,
    stale_age_ms: null,
    binding: { session: 's1', workspace: '/repo', worktree: 'wt1', daemon: 'http://127.0.0.1:8787', generation: 4 },
    sources,
    health: sources.health.data,
    ready: sources.ready.data,
    models: sources.models.data,
    capabilities: sources.capabilities.data,
    config,
    descriptors: sources.descriptors.data,
    connections: sources.connections.data,
    providers: sources.providers.data,
    parameters: sources.parameters.data,
    presets: sources.presets.data,
    instructions: sources.instructions.data,
    runtime: sources.runtime.data,
    provenance: sources.provenance.data,
    receipts: sources.receipts.data,
    policy: sources.policy.data,
    permissionAudit: sources.permissionAudit.data,
    approvals: sources.approvals.data,
    mcp: sources.mcp.data,
    tools: sources.tools.data,
    skills: sources.skills.data,
    hooks: sources.hooks.data,
    hookAudit: sources.hookAudit.data,
    memory: sources.memory.data,
    sessions: sources.sessions.data,
    sessionMeta: sources.sessionMeta.data,
    search: sources.search.data,
    transcript: sources.transcript.data,
    usage: sources.usage.data,
    sessionUsage: sources.sessionUsage.data,
    outcomes: sources.outcomes.data,
    proof: sources.proof.data,
    verification: sources.verification.data,
    checkpoints: sources.checkpoints.data,
    taskRuns: sources.taskRuns.data,
    tasks: sources.tasks.data,
    turns: sources.turns.data,
    diagnostics: sources.diagnostics.data,
    updater: sources.updater.data,
    identity: sources.identity.data,
    entitlements: sources.entitlements.data,
    browser: sources.browser.data,
    browserAdapters: sources.browserAdapters.data,
    browserSessions: sources.browserSessions.data,
    contextPins: sources.contextPins.data,
    efficiency: sources.efficiency.data,
    budget: sources.budget.data,
    semanticCapabilities: sources.semanticCapabilities.data,
    parallelSessions: sources.parallelSessions.data,
    enterprise: sources.enterprise.data,
    workers: sources.workers.data,
    index: sources.index.data,
    semantic: sources.semantic.data,
    terminals: sources.terminals.data,
    agents: sources.agents.data,
    board: sources.board.data,
    tournaments: sources.tournaments.data,
  };
  return { ...base, ...overrides };
}

export function edition8EmptySnapshot() {
  const emptySources = {};
  for (const key of Object.keys(sources)) {
    emptySources[key] = { state: 'unavailable', method: 'GET', path: `/${key}`, error: `${key} is unavailable in the empty fixture.`, observed_at_ms: 0, data: null };
  }
  const snapshot = edition8Snapshot({ sources: emptySources, stale: true, stale_age_ms: 120000 });
  for (const key of Object.keys(sources)) {
    const field = key;
    if (field in snapshot) snapshot[field] = null;
  }
  snapshot.config = null;
  snapshot.binding = null;
  return snapshot;
}

export function edition8StaleSnapshot() {
  return edition8Snapshot({ stale: true, stale_age_ms: 90_000, revision: '6' });
}

export function edition8ErrorSnapshot() {
  const snapshot = edition8Snapshot({ stale: true, stale_age_ms: 30_000 });
  snapshot.sources = {
    ...snapshot.sources,
    config: { ...snapshot.sources.config, state: 'error', error: 'conflict: revision moved', data: null },
    health: { ...snapshot.sources.health, state: 'error', error: 'transport: connect ECONNREFUSED', data: null },
  };
  snapshot.health = null;
  return snapshot;
}

export { NOW };
