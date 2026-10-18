// Deterministic Edition 5 UI fixtures. These are test inputs only: they are
// never loaded by the extension at runtime and every value carries its
// provenance so the forbidden fake-state checks can prove the UI does not
// turn an absence into a zero, green verdict or measurement.

export const SCHEMA = 'faktor-edition5-ui/v1';

export function richSnapshot(overrides = {}) {
  return {
    schema: SCHEMA,
    revision: 'r7',
    generated_at_ms: 1792256000000,
    stale: false,
    stale_age_ms: null,
    binding: {
      session: 's1',
      workspace: 'file:///work/faktor',
      worktree: '/work/faktor/.faktor/worktrees/candidate-1',
      daemon: '127.0.0.1:7777',
      generation: 4,
    },
    config: {
      revision: 'r7',
      saved_revision: 'r7',
      runtime_revision: 'r6',
      values: [
        { key: 'faktor.defaultProvider', value: 'ollama', source: 'user', locked: false, lock_source: null, apply_state: 'NextTask', descriptor_type: 'string', scope: 'user' },
        { key: 'faktor.defaultModel', value: 'qwen3:8b', source: 'user', locked: false, lock_source: null, apply_state: 'NextTask', descriptor_type: 'string', scope: 'user' },
        { key: 'routing_mode', value: 'pinned', source: 'daemon', locked: true, lock_source: 'operator policy', apply_state: 'Effective', descriptor_type: 'enum', scope: 'daemon' },
        { key: 'faktor.budgetTokens', value: 200000, source: 'user', locked: false, lock_source: null, apply_state: 'NextTask', descriptor_type: 'integer', scope: 'user' },
        { key: 'model.contextLimit', value: 32768, source: 'operator', locked: false, lock_source: null, apply_state: 'NextTask', descriptor_type: 'integer', scope: 'operator' },
        { key: 'temperature', value: 0, source: 'user', locked: false, lock_source: null, apply_state: 'NextCall', descriptor_type: 'number', scope: 'user' },
        { key: 'providers[0].id', value: 'ollama-local', source: 'machine', locked: false, lock_source: null, apply_state: 'Effective', descriptor_type: 'string', scope: 'machine' },
        { key: 'providers[0].kind', value: 'ollama', source: 'machine', locked: false, lock_source: null, apply_state: 'Effective', descriptor_type: 'enum', scope: 'machine' },
        { key: 'providers[0].base_url', value: 'http://127.0.0.1:11434', source: 'machine', locked: false, lock_source: null, apply_state: 'Effective', descriptor_type: 'string', scope: 'machine' },
        { key: 'providers[0].models', value: ['qwen3:8b'], source: 'machine', locked: false, lock_source: null, apply_state: 'Effective', descriptor_type: 'string_list', scope: 'machine' },
        { key: 'efficiency.failure_learning', value: true, source: 'daemon', locked: false, lock_source: null, apply_state: 'Effective', descriptor_type: 'boolean', scope: 'daemon' },
        { key: 'completion.level', value: 'local', source: 'user', locked: false, lock_source: null, apply_state: 'NextTask', descriptor_type: 'enum', scope: 'user' },
        { key: 'sandbox.network', value: 'strict', source: 'operator', locked: true, lock_source: 'managed policy', apply_state: 'Effective', descriptor_type: 'enum', scope: 'operator' },
      ],
      secrets: [
        { reference: 'env:OPENAI_API_KEY', bound: true, verified: true, vault_generation: 3, kind: 'environment', detail: 'bound at the daemon host' },
        { reference: 'vault:ollama-local', bound: true, verified: false, vault_generation: 3, kind: 'vault', detail: 'not verified for this endpoint' },
      ],
      managed_locks: ['sandbox.network', 'routing_mode'],
      updated_at_ms: 1792256000000,
    },
    models: {
      catalog: [
        { provider: 'ollama', model: 'qwen3:8b', context: 32768, max_output: 8192, tools: true, parallel_tools: 3, reasoning: true, thinking: true, vision: false, documents: false, streaming: true, structured_output: true, embeddings: false, capability_authority: 'known', capabilities_gated: false, availability: 'available', unavailable_reason: null, source: 'liveProbe', observed_at_ms: 1792255900000 },
        { provider: 'openai', model: 'gpt-next', context: 128000, max_output: 16384, tools: true, parallel_tools: 8, reasoning: true, thinking: true, vision: true, documents: true, streaming: true, structured_output: true, embeddings: true, capability_authority: 'declared_generic', capabilities_gated: false, availability: 'available', unavailable_reason: null, source: 'providerCatalog', observed_at_ms: 1792255900000 },
        { provider: 'custom', model: 'mystery-1', context: null, max_output: null, tools: null, parallel_tools: null, reasoning: null, thinking: null, vision: null, documents: null, streaming: null, structured_output: null, embeddings: null, capability_authority: 'unknown', capabilities_gated: true, availability: 'unavailable', unavailable_reason: 'No probe or exact-model declaration; gated tools/vision/schema remain disabled.', source: 'conservativeDefault', observed_at_ms: null },
      ],
      favorites: ['ollama/qwen3:8b'],
      recent: ['openai/gpt-next', 'ollama/qwen3:8b'],
      routing: { mode: 'pinned', requested: 'ollama/qwen3:8b', actual: 'ollama/qwen3:8b', fallback: [], next_task_revision: 'r7' },
      parameters: [
        { key: 'reasoning', kind: 'enum', min: null, max: null, choices: ['off', 'low', 'medium', 'high'], requires_exact_authority: true, unset_allowed: true, unit: null },
        { key: 'temperature', kind: 'number', min: 0, max: 2, choices: [], requires_exact_authority: false, unset_allowed: true, unit: null },
        { key: 'max_output', kind: 'integer', min: 1, max: 8192, choices: [], requires_exact_authority: false, unset_allowed: true, unit: 'tokens' },
        { key: 'stop', kind: 'string_list', min: null, max: 4, choices: [], requires_exact_authority: false, unset_allowed: true, unit: null },
      ],
      calls: [
        { call_id: 'call-1', provider: 'ollama', model: 'qwen3:8b', role: 'coder', route_reason: 'pinned', started_at_ms: 1792255950000, parameters: { temperature: 0, reasoning: 'medium' } },
      ],
      price: { list: null, declared: '0.10/1k', observed: '0.09/1k', quota: null, unknown_reason: 'List price and quota are not served for the local adapter.', basis: 'declared + observed, list unavailable' },
      quality: { declared: 0.8, observed: 0.74, samples: 12, basis: 'declared + observed samples' },
      probe: {
        last_observed_at_ms: 1792255900000,
        stages: [
          { stage: 'reachability', status: 'pass', detail: '127.0.0.1:11434 answered in 4 ms' },
          { stage: 'auth', status: 'not_required', detail: 'local Ollama has no credential' },
          { stage: 'catalog', status: 'pass', detail: '1 installed model' },
          { stage: 'capability', status: 'pass', detail: 'exact model qwen3:8b reported' },
        ],
        failure: null,
      },
    },
    providers: {
      connections: [
        { id: 'ollama-local', label: 'Work Ollama', kind: 'ollama', base_url: 'http://127.0.0.1:11434', credential_reference: null, credential_bound: true, headers: [], allow_loopback: true, transport_timeout_ms: 120000, proxy: null, tls: null, health: 'ready', health_detail: 'handshake observed', models: ['qwen3:8b'], source: 'machine', trust: 'local loopback opted in' },
        { id: 'openai-main', label: 'Hosted OpenAI', kind: 'openai_chat', base_url: 'https://api.openai.com/v1', credential_reference: 'env:OPENAI_API_KEY', credential_bound: true, headers: [{ name: 'X-Org', reference: 'env:OPENAI_ORG' }], allow_loopback: false, transport_timeout_ms: 60000, proxy: null, tls: 'system', health: 'auth_required', health_detail: '401 from the endpoint; rotate the credential reference', models: ['gpt-next'], source: 'machine', trust: 'operator configured' },
      ],
      local: {
        execution_location: 'daemon host 127.0.0.1 (loopback resolves on the daemon)',
        daemon_host: '127.0.0.1:7777',
        endpoint: 'http://127.0.0.1:11434',
        endpoint_class: 'loopback',
        loopback_authorized: true,
        lan_policy: 'private LAN endpoints remain refused by the current provider policy',
        declared_allocation: 32768,
        observed_allocation: 40960,
        observed_source: 'Ollama /api/ps liveProbe',
        keep_alive: '15m',
        unload_supported: true,
        embedding_boundary: 'no embedder configured; lexical/symbol retrieval remains active',
      },
      stages: [
        { stage: 'reachability', status: 'pass', detail: 'daemon host reached the endpoint' },
        { stage: 'auth', status: 'not_required', detail: 'no credential' },
        { stage: 'catalog', status: 'pass', detail: 'installed models listed' },
        { stage: 'capability', status: 'pass', detail: 'exact model probed; tool-call reliability is not certified' },
      ],
    },
    generation: {
      model: { provider: 'ollama', model: 'qwen3:8b' },
      parameters: [
        { key: 'temperature', kind: 'number', min: 0, max: 2, choices: [], requires_exact_authority: false, unset_allowed: true, unit: null },
        { key: 'reasoning', kind: 'enum', min: null, max: null, choices: ['off', 'low', 'medium', 'high'], requires_exact_authority: true, unset_allowed: true, unit: null },
      ],
      requested: { reasoning_effort: 'medium', thinking_budget: null, max_output: 4096, temperature: 0, top_p: null, top_k: null, frequency_penalty: null, presence_penalty: null, seed: null, stop: [], parallel_tool_calls: null, output_format: 'text', service_tier: null, cache_policy: null, stream: true },
      effective: { reasoning_effort: 'medium', thinking_budget: null, max_output: 4096, temperature: 0, top_p: null, top_k: null, frequency_penalty: null, presence_penalty: null, seed: null, stop: [], parallel_tool_calls: null, output_format: 'text', service_tier: null, cache_policy: null, stream: true },
      incompatible: [],
      apply_timing: 'Next task',
      preview_at_ms: 1792255950000,
    },
    agents: {
      presets: [
        { id: 'code', name: 'Code', kind: 'code', version: 'v3', instructions: 'Implement verified changes in an isolated candidate.', model: null, reasoning: 'medium', allowed_tools: ['read_file', 'edit_file', 'shell_run', 'test_run'], delegation: 'ask', maximum_children: 4, child_budget_micro: '500000', ownership: 'candidate worktree', handoff_schema: ['summary', 'criteria', 'artifacts'], enforced: true, ask_prompt_only: false, resolved_permissions: ['edit_file: ask', 'shell_run: ask', 'managed deny: owner checkout'] },
        { id: 'ask', name: 'Ask', kind: 'ask', version: 'v2', instructions: 'Explore read-only; never dispatch a write.', model: null, reasoning: 'low', allowed_tools: ['read_file', 'search'], delegation: 'off', maximum_children: 0, child_budget_micro: null, ownership: 'none', handoff_schema: ['summary'], enforced: true, ask_prompt_only: false, resolved_permissions: ['read_file: allow', 'edit_file: deny', 'shell_run: deny'] },
        { id: 'plan', name: 'Plan', kind: 'plan', version: 'v1', instructions: 'Produce a named, versioned plan artifact.', model: null, reasoning: 'medium', allowed_tools: ['read_file', 'write_plan'], delegation: 'off', maximum_children: 0, child_budget_micro: null, ownership: 'plan artifact', handoff_schema: ['plan'], enforced: true, ask_prompt_only: false, resolved_permissions: ['write_plan: allow', 'edit_file: deny'] },
        { id: 'debug', name: 'Debug', kind: 'debug', version: 'v1', instructions: 'Reproduce and diagnose before editing.', model: null, reasoning: 'high', allowed_tools: ['read_file', 'shell_run', 'edit_file'], delegation: 'permitted', maximum_children: 2, child_budget_micro: '250000', ownership: 'candidate worktree', handoff_schema: ['summary', 'artifacts'], enforced: true, ask_prompt_only: false, resolved_permissions: ['shell_run: ask', 'edit_file: ask'] },
        { id: 'review', name: 'Review', kind: 'review', version: 'v1', instructions: 'Review changes without editing.', model: null, reasoning: 'medium', allowed_tools: ['read_file', 'read_diff'], delegation: 'off', maximum_children: 0, child_budget_micro: null, ownership: 'none', handoff_schema: ['findings'], enforced: true, ask_prompt_only: false, resolved_permissions: ['edit_file: deny'] },
        { id: 'custom-1', name: 'Docs helper', kind: 'custom', version: 'v1', instructions: 'Draft documentation updates.', model: 'openai/gpt-next', reasoning: 'medium', allowed_tools: ['read_file', 'edit_file'], delegation: 'ask', maximum_children: 1, child_budget_micro: '100000', ownership: 'candidate worktree', handoff_schema: ['summary'], enforced: false, ask_prompt_only: true, resolved_permissions: ['edit_file: ask', 'managed deny retained'] },
      ],
      roles: [
        { role: 'planner', provider: 'ollama', model: 'qwen3:8b', inherited: true, capability_authority: 'known' },
        { role: 'coder', provider: 'ollama', model: 'qwen3:8b', inherited: true, capability_authority: 'known' },
        { role: 'verifier', provider: 'openai', model: 'gpt-next', inherited: false, capability_authority: 'declared_generic' },
        { role: 'summarizer', provider: 'ollama', model: 'qwen3:8b', inherited: false, capability_authority: 'known' },
      ],
      tree: [
        { agent_id: 'parent-1', parent_id: null, role: 'coder', state: 'active', provider: 'ollama', model: 'qwen3:8b', capability_authority: 'known', ownership: 'candidate worktree', budget: { cap_micro: '5000000', open_micro: '1000000', settled_micro: '400000', uncertain_micro: '0' }, generation: 4, delivery_point: null, handoff: null, artifacts: [] },
        { agent_id: 'child-1', parent_id: 'parent-1', role: 'verifier', state: 'waiting', provider: 'openai', model: 'gpt-next', capability_authority: 'declared_generic', ownership: 'shared candidate evidence', budget: { cap_micro: '500000', open_micro: '200000', settled_micro: '0', uncertain_micro: '0' }, generation: 4, delivery_point: 'next tool boundary', handoff: 'Child output is evidence; parent obligations remain.', artifacts: ['evidence:9'] },
      ],
      concurrency: { limit: 4, active: 2, shared_budget_micro: '5000000' },
    },
    context: {
      usage: {
        capacity: 32768,
        used: 18400,
        output_reserve: 4096,
        safety_reserve: 2048,
        headroom: 8224,
        basis: 'estimated',
        categories: [
          { category: 'system', tokens: 3200, basis: 'estimated' },
          { category: 'rules', tokens: 1400, basis: 'estimated' },
          { category: 'tools', tokens: 900, basis: 'estimated' },
          { category: 'history', tokens: 6400, basis: 'estimated' },
          { category: 'files', tokens: 5500, basis: 'estimated' },
          { category: 'retrieval', tokens: 1000, basis: 'estimated' },
        ],
      },
      manifest: {
        revision: 'ctx-12',
        sources: [
          { id: 'src-1', kind: 'file', uri: 'file:///work/faktor/.faktor/worktrees/candidate-1/src/lib.rs', range: null, version: 'sha256:aaa1', content_hash: 'sha256:aaa1', bytes: 18420, worktree: '/work/faktor/.faktor/worktrees/candidate-1', inclusion_reason: 'mentioned by the user', included: true, pinned: true, excluded: false, failed_reason: null },
          { id: 'src-2', kind: 'selection', uri: 'file:///work/faktor/.faktor/worktrees/candidate-1/src/lib.rs', range: '10:24', version: 'sha256:bbb2', content_hash: 'sha256:bbb2', bytes: 640, worktree: '/work/faktor/.faktor/worktrees/candidate-1', inclusion_reason: 'current editor selection', included: true, pinned: false, excluded: false, failed_reason: null },
          { id: 'src-3', kind: 'diagnostic', uri: 'file:///work/faktor/.faktor/worktrees/candidate-1/src/lib.rs', range: '18:1', version: 'sha256:ccc3', content_hash: 'sha256:ccc3', bytes: 220, worktree: '/work/faktor/.faktor/worktrees/candidate-1', inclusion_reason: 'Open Problems entry', included: true, pinned: false, excluded: false, failed_reason: null },
          { id: 'src-4', kind: 'attachment', uri: 'file:///work/faktor/.faktor/worktrees/candidate-1/trace.log', range: null, version: null, content_hash: null, bytes: 9000000, worktree: '/work/faktor/.faktor/worktrees/candidate-1', inclusion_reason: 'attached by the user', included: false, pinned: false, excluded: false, failed_reason: 'Oversize: 9,000,000 bytes exceeds the 8,388,608-byte attachment gate.' },
        ],
        captured_at_ms: 1792255950000,
      },
      overflow: null,
      retrieval: { strategy: 'hybrid', embedding_provider: null, embedding_model: null, index_coverage: 0.87, index_fresh: true, fallback: 'lexical/symbol (no embedder configured)' },
      editor_context: 'selection',
      ignore_patterns: ['target/**', '.git/**'],
      protected_patterns: ['AGENTS.md', 'docs/specs/**'],
    },
    compaction: {
      plan: {
        expected_revision: 'ctx-12',
        retained_facts: ['contract v3 (constraints, invariants, risks, obligations)', 'candidate identity sha256:candidate-1', 'policy revision r7', 'pending approval req-1', 'unknown effect op-9', 'evidence refs evidence:3, evidence:7'],
        summary_model: 'ollama/qwen3:8b',
        egress: 'daemon-local (loopback)',
        cost_micro: null,
        before_tokens: 18400,
        minimum_reduction: 0.3,
        pending_operations: ['req-1', 'op-9'],
        candidate_identity: 'sha256:candidate-1',
      },
      operation: {
        id: 'compact-1',
        state: 'Applied',
        error: null,
        measured_before: 18400,
        measured_after: 9800,
        summary_version: 'sum-3',
        minimum_reduction: 0.3,
        reduction_fraction: 0.467,
      },
      threshold: 0.8,
    },
    instructions: {
      sources: [
        { id: 'user', origin: 'User settings', scope: 'user', path: null, enabled: true, locked: false, lock_source: null, version: 'u4', hash: 'sha256:u4', content: 'Prefer verified changes.', plugins: [] },
        { id: 'project', origin: 'AGENTS.md', scope: 'project', path: 'AGENTS.md', enabled: true, locked: false, lock_source: null, version: 'p2', hash: 'sha256:p2', content: 'Follow the Faktor repository guide.', plugins: [] },
        { id: 'rule-1', origin: '.faktor/rules/rust.md', scope: 'project', path: '.faktor/rules/rust.md', enabled: true, locked: false, lock_source: null, version: 'r1', hash: 'sha256:r1', content: 'For **/*.rs use cargo fmt.', plugins: ['globs'] },
        { id: 'managed', origin: 'Managed policy', scope: 'machine', path: null, enabled: true, locked: true, lock_source: 'enterprise policy v2', version: 'm2', hash: 'sha256:m2', content: 'Owner checkout is always denied for writes.', plugins: [] },
      ],
      effective: { version: 'eff-5', hash: 'sha256:eff5', merged: 'Managed policy\nUser settings\nAGENTS.md\n.faktor/rules/rust.md', order: ['managed', 'user', 'project', 'rule-1'] },
      conflicts: [{ sources: ['project', 'rule-1'], detail: 'Both set a formatting instruction; the scoped rule wins for **/*.rs and the project text remains for other paths.' }],
      templates: [{ name: 'explain', variables: ['path', 'question'] }, { name: 'fix', variables: ['path', 'error'] }],
    },
    memory: {
      records: [
        { id: 'mem-1', scope: 'project', kind: 'failure', text: 'The legacy parser rejects empty heredocs.', source: 'learned failure', evidence_ref: 'evidence:3', created_at_ms: 1792255000000, stopped_future_use_at_revision: null, retained_prior_artifacts: [] },
        { id: 'mem-2', scope: 'user', kind: 'preference', text: 'Prefer small, reviewable commits.', source: 'user instruction', evidence_ref: null, created_at_ms: 1792254000000, stopped_future_use_at_revision: null, retained_prior_artifacts: [] },
      ],
      proposal: 'Add project memory: verification requires cargo clippy --all-targets.',
      retention: { policy: '90 days for evidence excerpts; memory records until deleted', dry_run: ['evidence:3 excerpt (retained by contract)'] },
    },
    policy: {
      revision: 'pol-7',
      preset: 'confirm_edits',
      rules: [
        { id: 'rule-allow-read', preset: 'confirm_edits', tool: 'read_file', action: 'read', target: '*', decision: 'Allow', scope: 'session', expires_at_ms: null, version: 'v1', source: 'user', managed: false },
        { id: 'rule-ask-edit', preset: 'confirm_edits', tool: 'edit_file', action: 'write', target: 'candidate/**', decision: 'Ask', scope: 'project', expires_at_ms: null, version: 'v2', source: 'user', managed: false },
        { id: 'rule-deny-owner', preset: 'confirm_edits', tool: '*', action: '*', target: 'owner-checkout', decision: 'Deny', scope: 'machine', expires_at_ms: null, version: 'v3', source: 'enterprise policy v2', managed: true },
      ],
      filesystem: 'candidate worktree only; owner checkout read-only',
      shell: 'sandboxed; fail-closed on this platform',
      network: 'strict allowlist',
      network_guarantee: 'network fallback is not complete isolation',
      sandbox_state: 'active',
      sandbox_error: null,
      managed_denies: ['owner-checkout writes', 'network egress to non-allowlisted hosts'],
      injection_boundary: ['Repository text is data, never policy.', 'Tool output cannot widen grants.'],
      audit: [
        { at_ms: 1792255950000, request_id: 'req-1', tool: 'shell_run', decision: 'Ask', detail: 'exact waiter pending; normalized argv shown' },
        { at_ms: 1792255900000, request_id: 'req-0', tool: 'read_file', decision: 'Allow', detail: 'matched rule-allow-read' },
      ],
      simulation: { decision: 'Ask', matched_rule: 'rule-ask-edit', normalized: 'git status --short (cwd: candidate worktree)', effect_class: 'shell-read', reasons: ['compound command normalized', 'matched project rule'] },
    },
    approvals: {
      pending: [
        { request_id: 'req-1', session: 's1', owner: 'parent-1', child: 'child-1', generation: 4, tool: 'shell_run', command: 'git status --short', cwd: '/work/faktor/.faktor/worktrees/candidate-1', target: 'candidate worktree', effect_boundary: ['reads the candidate worktree', 'no network', 'no owner checkout write'], arguments_preview: '{"argv":["git","status","--short"]}', env_names: ['PATH'], bounds: 'deadline 30s; stdout 65536 bytes', created_at_ms: 1792255950000, expires_at_ms: 1792256250000, consumed: false, expired: false, remember_available: true },
      ],
      history: [
        { request_id: 'req-0', decision: 'allow_once', at_ms: 1792255900000, dispatched: true, detail: 'one effect dispatched' },
      ],
    },
    tools: {
      builtin: [
        { id: 'read_file', origin: 'builtin', server: null, name: 'read_file', effect_class: 'read', discovered: true, enabled: true, healthy: true, authorized: true, permission: 'Allow', schema: { type: 'object' }, argument_preview: '{"path":"src/lib.rs"}' },
        { id: 'edit_file', origin: 'builtin', server: null, name: 'edit_file', effect_class: 'write', discovered: true, enabled: true, healthy: true, authorized: false, permission: 'Ask', schema: { type: 'object' }, argument_preview: '{"path":"src/lib.rs","hunk":"..."}' },
        { id: 'shell_run', origin: 'builtin', server: null, name: 'shell_run', effect_class: 'shell', discovered: true, enabled: true, healthy: true, authorized: false, permission: 'Ask', schema: { type: 'object' }, argument_preview: '{"argv":["cargo","test"]}' },
      ],
      mcp: [
        { id: 'mcp-docs', name: 'docs', transport: 'stdio', command: '/usr/local/bin/mcp-docs', args: ['--stdio'], env_references: ['env:DOCS_TOKEN'], source: 'machine settings', trusted: true, health: 'ready', health_detail: 'handshake observed', auth_state: 'ready', error: null, stdout_cap: 1048576, stderr_cap: 262144, timeout_ms: 30000, tool_count: 4, logs: ['ready in 41 ms'], resources: ['docs://spec'], prompts: ['summarize'], remote_supported: true },
        { id: 'mcp-hosted', name: 'hosted', transport: 'streamable_http', command: 'https://example.invalid/mcp', args: [], env_references: ['vault:MCP_CLIENT'], source: 'operator', trusted: true, health: 'auth_required', health_detail: 'OAuth token missing', auth_state: 'required', error: null, stdout_cap: null, stderr_cap: null, timeout_ms: 30000, tool_count: 0, logs: [], resources: [], prompts: [], remote_supported: true },
      ],
      tool_loading: 'deferred',
      export_available: true,
    },
    skills: {
      skills: [
        { id: 'skill-rust', name: 'rust-review', version: '1.2.0', source: 'trusted store', trust: 'verified publisher', enabled: true, invocation: 'agent_selectable', capabilities: ['read_file'], files: ['SKILL.md'], manifest: 'name: rust-review\nversion: 1.2.0', version_diff: '+ require cargo fmt --check', dependencies_available: true, prior_version: '1.1.0' },
        { id: 'skill-docs', name: 'docs-writer', version: '0.4.0', source: 'repository (.faktor/skills)', trust: 'project content (not auto-started)', enabled: false, invocation: 'disabled', capabilities: ['edit_file'], files: ['SKILL.md', 'setup.sh'], manifest: 'name: docs-writer\nsetup: setup.sh', version_diff: null, dependencies_available: false, prior_version: null },
      ],
      commands: [
        { name: 'review', scope: 'project', effect: 'insert_text', template: 'Review {{path}} for {{focus}}', variables: ['path', 'focus'] },
        { name: 'fix-tests', scope: 'user', effect: 'start_task', template: 'Fix the failing tests in {{path}}', variables: ['path'] },
      ],
      hooks: [
        { id: 'hook-pre-tool', event: 'PreToolUse', executable: '/usr/local/bin/faktor-hook', args: ['--policy'], match: 'tool=shell_run', env_allowlist: ['PATH'], deadline_ms: 5000, stdout_cap: 65536, stderr_cap: 65536, failure_policy: 'FailClosed', version: 'h2', last_test: { outcome: 'Verdict', exit_state: '0', verdict: 'Allow', stdout: 'allow', stderr: '', audit_ref: 'evidence:11', spec_hash: 'sha256:h2', at_ms: 1792255900000 } },
      ],
      hook_events: ['SessionStart', 'UserPromptSubmit', 'PreToolUse', 'PostToolUse', 'PostToolUseFailure', 'PermissionRequest', 'Notification', 'Stop', 'SubagentStart', 'SubagentStop', 'PreCompact', 'PostCompact', 'SessionEnd', 'PreModelCall', 'PostModelCall', 'PreEdit', 'PostEdit'],
    },
    terminal: {
      processes: [
        { id: 'term-1', pid: 4242, owner: 'parent-1', binding: 's1/worktree-1/daemon-4', live: true, state: 'running', cwd: '/work/faktor/.faktor/worktrees/candidate-1', sandbox_profile: 'sandboxed shell', command: 'cargo test --workspace', output_head: 'Compiling faktor-core…', output_tail: 'test result: ok. 42 passed', stdout_bytes: 184000, stderr_bytes: 0, artifact_ref: 'evidence:12', effect_status: 'unknown', handle_generation: 4 },
      ],
      profiles: [{ name: 'sandbox', detail: 'filesystem candidate only; network strict' }],
      queue: [{ id: 'q1', text: 'After this run, update the changelog.', state: 'queued_next' }],
    },
    browser: {
      adapters: [
        { id: 'chromium', name: 'Local Chromium', available: true, unavailable_reason: null, account: null, machine: 'daemon host' },
        { id: 'remote-browser', name: 'Remote browser', available: false, unavailable_reason: 'No advertised remote adapter; unavailable support is labelled, not shown as an idle tab.', account: null, machine: null },
      ],
      sessions: [
        { id: 'b1', url: 'http://127.0.0.1:3000/', account: null, machine: 'daemon host', network_scope: 'loopback only', screenshot_ref: 'evidence:13', console: [], network: ['GET / 200'] },
      ],
    },
    budget: {
      token_cap: 200000,
      cost_cap_micro: '5000000',
      attempts: 3,
      base_delay_ms: 500,
      max_delay_ms: 8000,
      jitter: 0.2,
      deadline_ms: 1800000,
      loop_limits: { steps: 64, consecutive_failures: 4 },
      ledger: { open_micro: '1000000', settled_micro: '400000', refunded_micro: '0', uncertain_micro: '50000' },
      warning: 'Soft warning: 80% of the money cap is reserved; admission still succeeds until the hard cap.',
      enforced: true,
      rate_limit: { cause: 'provider 429', retry_at_ms: null, attempt: 2, max_attempts: 3 },
      revision: 'budget-r3',
    },
    run: {
      state: 'waiting',
      timeline: [
        { id: 't1', kind: 'tool', state: 'completed', detail: 'read_file src/lib.rs', at_ms: 1792255950000, effect_status: 'settled', call: null },
        { id: 't2', kind: 'model', state: 'started', detail: 'coder call', at_ms: 1792255960000, effect_status: null, call: { call_id: 'call-1', provider: 'ollama', model: 'qwen3:8b', role: 'coder', route_reason: 'pinned', started_at_ms: 1792255960000, parameters: { temperature: 0 } } },
        { id: 't3', kind: 'tool', state: 'unknown', detail: 'shell_run effect boundary unknown after disconnect', at_ms: 1792255970000, effect_status: 'unknown', call: null },
      ],
      queue: [{ id: 'q1', text: 'After this run, update the changelog.', state: 'queued_next' }],
      display: 'balanced',
      attention: ['Approval req-1 is pending', 'Effect op-9 is unknown until reconciled'],
      stale: false,
    },
    changes: {
      candidate: 'sha256:candidate-1',
      base: 'sha256:base-1',
      files: [
        { path: 'src/lib.rs', status: 'modified', additions: 24, deletions: 3, hunks: ['h1', 'h2'] },
        { path: 'tests/lib.rs', status: 'added', additions: 40, deletions: 0, hunks: ['h1'] },
      ],
      comments: [{ id: 'c1', path: 'src/lib.rs', line: 18, body: 'Check the empty-input branch.', version: 'v1' }],
      acceptance: ['cargo test --workspace passes', 'no new clippy warnings'],
      manual_edit_conflict: null,
    },
    checkpoints: [
      { id: 'cp-1', created_at_ms: 1792255000000, before_hash: 'sha256:b1', after_hash: 'sha256:a1', content_available: true, coverage: 'files: src/lib.rs, tests/lib.rs; conversation range covered', uncovered_effects: ['shell: cargo test (no rollback)'], redo_supported: true, diff_supported: true, files: ['src/lib.rs', 'tests/lib.rs'], conversation_range: 'seq 1..42' },
      { id: 'cp-0', created_at_ms: 1792254000000, before_hash: 'sha256:b0', after_hash: null, content_available: false, coverage: 'hash only; after-content blobs absent', uncovered_effects: ['network fetch', 'SCM push'], redo_supported: false, diff_supported: false, files: [], conversation_range: null },
    ],
    history: {
      sessions: [
        { id: 's1', title: 'Fix the parser', provider: 'ollama', model: 'qwen3:8b', state: 'waiting', worktree: 'candidate-1', outcome: 'pending', pinned: true, archived: false, updated_at_ms: 1792256000000, active_run: true },
        { id: 's2', title: 'Review the CLI', provider: 'openai', model: 'gpt-next', state: 'completed', worktree: 'candidate-2', outcome: 'verified', pinned: false, archived: false, updated_at_ms: 1792255000000, active_run: false },
        { id: 's3', title: 'Old experiment', provider: 'ollama', model: 'qwen3:8b', state: 'completed', worktree: 'candidate-3', outcome: 'unverified', pinned: false, archived: true, updated_at_ms: 1792253000000, active_run: false },
      ],
      next_cursor: 'cursor-2',
      total: 12,
      search: { query: 'parser', mode: 'transcript-find', results: [{ session: 's2', seq: 17, excerpt: 'parser rejected an empty heredoc' }], next_cursor: 'find-2' },
      archived_is_deleted: false,
    },
    usage: {
      attempted_spend_micro: '1450000',
      verified_landed_changes: 2,
      vcsc_micro: '725000',
      vcsc_state: 'measured',
      cohort: 'all worktrees',
      period: 'last 30 days',
      denominator: 'verified landed changes',
      tokens_in: 120000,
      tokens_out: 24000,
      cache_read: 8000,
      cache_write: 2000,
      provider_quota: 'provider quota: 1,000,000 tokens',
      local_estimate: 'local estimate: 144,000 tokens',
      observed_charge: 'observed charge: 1,450,000 micro-USD',
    },
    outcomes: {
      verified: 2,
      unverified: 1,
      pending: 1,
      receipts: [{ id: 'receipt-1', candidate: 'sha256:candidate-0', verified_at_ms: 1792254800000, landed: true, detail: 'matching landed candidate' }],
    },
    efficiency: {
      flags: [
        { key: 'failure_learning', enabled: true, provenance: 'daemon config', measured: '12 matched failures', sample_count: 12 },
        { key: 'ccr', enabled: true, provenance: 'daemon config', measured: null, sample_count: null },
        { key: 'typed_handoff', enabled: true, provenance: 'daemon config', measured: '2 handoffs', sample_count: 2 },
        { key: 'semantic_context', enabled: false, provenance: 'config; no embedder', measured: null, sample_count: null },
        { key: 'rework_routing', enabled: false, provenance: 'daemon config', measured: null, sample_count: null },
      ],
      retrieval: 'hybrid',
      fallback: 'lexical/symbol (no embedder)',
      aliases: ['efficiency.semantic_context', 'efficiency.information_gain'],
    },
    settings: {
      search: [
        { key: 'routing_mode', effective: 'pinned', source: 'operator policy', locked: true, scope: 'daemon' },
        { key: 'sandbox.network', effective: 'strict', source: 'managed policy', locked: true, scope: 'operator' },
        { key: 'faktor.defaultModel', effective: 'qwen3:8b', source: 'user', locked: false, scope: 'user' },
      ],
      import_export: { export_available: true, import_available: true, schema: 'faktor-config/v2' },
      telemetry: { categories: ['crash', 'usage-counts'], enabled: false },
    },
    diagnostics: {
      readiness: 'ready',
      readiness_detail: 'store recovered; protocol compatible',
      version: '0.9.1',
      runtime_path: '/opt/faktor/current/bin/faktor-cli',
      checksum: 'sha256:abc123',
      checksum_verified: true,
      signature_verified: false,
      trust: 'checksum matches the local record; publisher signature not verified',
      protocol_version: 1,
      protocol_compatible: true,
      connectivity: { state: 'connected', detail: 'SSE cursor 412', retryable: false },
      uncertain_admissions: [{ operation_id: 'op-9', receipt: null, retry_eligible: false, effect_status: 'unknown' }],
      export_preview: { categories: ['readiness', 'versions', 'log-tail'], omitted: ['secrets', 'prompts', 'transcripts'], bytes: 42000, redacted: true },
      log_tail: ['ready', 'stream resumed at 412'],
    },
    binding_view: {
      editor_host: 'local VS Code (macOS)',
      workspace_uri: 'file:///work/faktor',
      daemon_host: '127.0.0.1:7777 (same machine)',
      model_endpoint: 'http://127.0.0.1:11434 (resolves on the daemon)',
      embedding_endpoint: null,
      vault_location: 'macOS keychain (daemon host)',
      worker_location: 'local daemon',
      data_boundary: 'candidate and model traffic stay on the daemon host; no hosted egress for the selected route',
      generation: 4,
      stale_handles: [],
    },
    proof: {
      outcome: 'pending',
      required_total: 2,
      required_passed: 1,
      criteria: [
        { key: 'cargo test --workspace', requirement: 'required', verdict: 'pass', binding: 'required_check:unit', evidence_refs: ['evidence:20'], detail: '42 passed' },
        { key: 'no new clippy warnings', requirement: 'required', verdict: 'unavailable', binding: 'required_check:clippy', evidence_refs: [], detail: 'no evidence receipt yet' },
      ],
      claims_complete: true,
      candidate: 'sha256:candidate-1',
      verified_candidate: null,
      landed_candidate: null,
      finish_requested: 'pr',
      finish_receipt: null,
      blockers: ['Missing required evidence for no new clippy warnings', 'No SCM target is configured, so the requested PR cannot be created'],
      receipts: [{ id: 'evidence:20', label: 'cargo test output', kind: 'check' }],
    },
    ...overrides,
  };
}

export function emptySnapshot() {
  return {
    schema: SCHEMA,
    revision: '0',
    generated_at_ms: 1792256000000,
    stale: true,
    stale_age_ms: 120000,
    binding: null,
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

export function staleSnapshot() {
  const snapshot = richSnapshot();
  snapshot.stale = true;
  snapshot.stale_age_ms = 45000;
  return snapshot;
}

export function unknownCapabilitySnapshot() {
  return richSnapshot({
    models: {
      ...richSnapshot().models,
      catalog: [richSnapshot().models.catalog[2]],
      favorites: [],
      routing: { mode: 'pinned', requested: 'custom/mystery-1', actual: 'custom/mystery-1', fallback: [], next_task_revision: null },
    },
  });
}

export function contextNullSnapshot() {
  const snapshot = richSnapshot();
  snapshot.context.usage = null;
  return snapshot;
}

export function zeroDenominatorSnapshot() {
  const snapshot = richSnapshot();
  snapshot.usage = {
    ...snapshot.usage,
    verified_landed_changes: 0,
    vcsc_micro: null,
    vcsc_state: 'unavailable',
    denominator: 'verified landed changes (denominator is zero)',
  };
  return snapshot;
}

export function failedCompactionSnapshot() {
  const snapshot = richSnapshot();
  snapshot.compaction.operation = {
    id: 'compact-2',
    state: 'Refused',
    error: 'Reduction 1.2% is below the configured 30% minimum; the prior basis is preserved.',
    measured_before: 18400,
    measured_after: 18180,
    summary_version: null,
    minimum_reduction: 0.3,
    reduction_fraction: 0.012,
  };
  return snapshot;
}

export function verifiedSnapshot() {
  const base = richSnapshot();
  base.proof = {
    outcome: 'verified',
    required_total: 2,
    required_passed: 2,
    criteria: [
      { key: 'cargo test --workspace', requirement: 'required', verdict: 'pass', binding: 'required_check:unit', evidence_refs: ['evidence:20'], detail: '42 passed' },
      { key: 'no new clippy warnings', requirement: 'required', verdict: 'pass', binding: 'required_check:clippy', evidence_refs: ['evidence:21'], detail: 'clean' },
    ],
    claims_complete: true,
    candidate: 'sha256:candidate-1',
    verified_candidate: 'sha256:candidate-1',
    landed_candidate: 'sha256:candidate-1',
    finish_requested: 'commit',
    finish_receipt: 'finish-1',
    blockers: [],
    receipts: [
      { id: 'evidence:20', label: 'cargo test output', kind: 'check' },
      { id: 'evidence:21', label: 'clippy output', kind: 'check' },
    ],
  };
  return base;
}

export function contextUsageExample() {
  return { capacity: 32768, input: 18400, outputReserve: 4096, safetyReserve: 2048, headroom: 8224 };
}
