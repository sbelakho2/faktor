// Edition 5 native contract (client side). Schema: faktor-edition5-ui/v1.
// The read model is served by GET /native/ui/edition5?session=<id>; the
// command DTOs drive the typed endpoints listed in docs/edition5-native-contract.md.
// Every nullable member is an explicit Unavailable state: this client never
// fabricates a zero, a green verdict, a measured value or a saved state.

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export class E5ProtocolError extends Error {
  readonly path: string;
  constructor(path: string, detail: string) {
    super(`edition5 DTO violation at ${path}: ${detail}`);
    this.name = 'E5ProtocolError';
    this.path = path;
  }
}

export type E5CapabilityAuthority = 'known' | 'declared_generic' | 'unknown';
export type E5ApplyState =
  | 'Effective'
  | 'NextCall'
  | 'NextTask'
  | 'NewSession'
  | 'RestartRequired'
  | 'Failed'
  | 'Saved'
  | 'Staged';
export type E5Availability = 'available' | 'unavailable' | 'unknown';

export interface E5BindingTuple {
  readonly session: string | null;
  readonly workspace: string | null;
  readonly worktree: string | null;
  readonly daemon: string | null;
  readonly generation: number | null;
}

export interface E5ConfigValue {
  readonly key: string;
  readonly value: Json;
  readonly source: string;
  readonly locked: boolean;
  readonly lock_source: string | null;
  readonly apply_state: E5ApplyState;
  readonly descriptor_type: string;
  readonly scope: string;
}

export interface E5SecretReference {
  readonly reference: string;
  readonly bound: boolean;
  readonly verified: boolean;
  readonly vault_generation: number | null;
  readonly kind: string;
  readonly detail: string | null;
}

export interface E5ConfigSnapshot {
  readonly revision: string;
  readonly saved_revision: string | null;
  readonly runtime_revision: string | null;
  readonly values: readonly E5ConfigValue[];
  readonly secrets: readonly E5SecretReference[];
  readonly managed_locks: readonly string[];
  readonly updated_at_ms: number | null;
}

export interface E5ModelCapability {
  readonly provider: string;
  readonly model: string;
  readonly context: number | null;
  readonly max_output: number | null;
  readonly tools: boolean | null;
  readonly parallel_tools: number | null;
  readonly reasoning: boolean | null;
  readonly thinking: boolean | null;
  readonly vision: boolean | null;
  readonly documents: boolean | null;
  readonly streaming: boolean | null;
  readonly structured_output: boolean | null;
  readonly embeddings: boolean | null;
  readonly capability_authority: E5CapabilityAuthority;
  readonly capabilities_gated: boolean;
  readonly availability: E5Availability;
  readonly unavailable_reason: string | null;
  readonly source: string | null;
  readonly observed_at_ms: number | null;
}

export interface E5ModelParameter {
  readonly key: string;
  readonly kind: 'integer' | 'number' | 'boolean' | 'enum' | 'string' | 'string_list' | 'json';
  readonly min: number | null;
  readonly max: number | null;
  readonly choices: readonly string[];
  readonly requires_exact_authority: boolean;
  readonly unset_allowed: boolean;
  readonly unit: string | null;
}

export interface E5CallIdentity {
  readonly call_id: string;
  readonly provider: string;
  readonly model: string;
  readonly role: string;
  readonly route_reason: string;
  readonly started_at_ms: number;
  readonly parameters: Readonly<Record<string, Json>>;
}

export interface E5PriceSheet {
  readonly list: string | null;
  readonly declared: string | null;
  readonly observed: string | null;
  readonly quota: string | null;
  readonly unknown_reason: string | null;
  readonly basis: string;
}

export interface E5ModelsView {
  readonly catalog: readonly E5ModelCapability[];
  readonly favorites: readonly string[];
  readonly recent: readonly string[];
  readonly routing: {
    readonly mode: string;
    readonly requested: string;
    readonly actual: string;
    readonly fallback: readonly string[];
    readonly next_task_revision: string | null;
  } | null;
  readonly parameters: readonly E5ModelParameter[];
  readonly calls: readonly E5CallIdentity[];
  readonly price: E5PriceSheet | null;
  readonly quality: {
    readonly declared: number | null;
    readonly observed: number | null;
    readonly samples: number | null;
    readonly basis: string;
  } | null;
  readonly probe: {
    readonly last_observed_at_ms: number | null;
    readonly stages: readonly { readonly stage: string; readonly status: string; readonly detail: string | null }[];
    readonly failure: string | null;
  } | null;
}

export interface E5ProviderConnection {
  readonly id: string;
  readonly label: string;
  readonly kind: string;
  readonly base_url: string;
  readonly credential_reference: string | null;
  readonly credential_bound: boolean;
  readonly headers: readonly { readonly name: string; readonly reference: string }[];
  readonly allow_loopback: boolean;
  readonly transport_timeout_ms: number | null;
  readonly proxy: string | null;
  readonly tls: string | null;
  readonly health: string;
  readonly health_detail: string | null;
  readonly models: readonly string[];
  readonly source: string;
  readonly trust: string;
}

export interface E5ProvidersView {
  readonly connections: readonly E5ProviderConnection[];
  readonly local: {
    readonly execution_location: string | null;
    readonly daemon_host: string | null;
    readonly endpoint: string | null;
    readonly endpoint_class: string;
    readonly loopback_authorized: boolean;
    readonly lan_policy: string | null;
    readonly declared_allocation: number | null;
    readonly observed_allocation: number | null;
    readonly observed_source: string | null;
    readonly keep_alive: string | null;
    readonly unload_supported: boolean;
    readonly embedding_boundary: string | null;
  } | null;
  readonly stages: readonly { readonly stage: string; readonly status: string; readonly detail: string | null }[];
}

export interface E5GenerationBundle {
  readonly reasoning_effort: string | null;
  readonly thinking_budget: number | null;
  readonly max_output: number | null;
  readonly temperature: number | null;
  readonly top_p: number | null;
  readonly top_k: number | null;
  readonly frequency_penalty: number | null;
  readonly presence_penalty: number | null;
  readonly seed: number | null;
  readonly stop: readonly string[];
  readonly parallel_tool_calls: number | null;
  readonly output_format: string | null;
  readonly service_tier: string | null;
  readonly cache_policy: string | null;
  readonly stream: boolean | null;
}

export interface E5GenerationView {
  readonly model: { readonly provider: string; readonly model: string } | null;
  readonly parameters: readonly E5ModelParameter[];
  readonly requested: E5GenerationBundle | null;
  readonly effective: E5GenerationBundle | null;
  readonly incompatible: readonly { readonly key: string; readonly reason: string }[];
  readonly apply_timing: string;
  readonly preview_at_ms: number | null;
}

export interface E5AgentPreset {
  readonly id: string;
  readonly name: string;
  readonly kind: string;
  readonly version: string;
  readonly instructions: string;
  readonly model: string | null;
  readonly reasoning: string | null;
  readonly allowed_tools: readonly string[];
  readonly delegation: string;
  readonly maximum_children: number | null;
  readonly child_budget_micro: string | null;
  readonly ownership: string;
  readonly handoff_schema: readonly string[];
  readonly enforced: boolean;
  readonly ask_prompt_only: boolean;
  readonly resolved_permissions: readonly string[];
}

export interface E5AgentTreeNode {
  readonly agent_id: string;
  readonly parent_id: string | null;
  readonly role: string;
  readonly state: string;
  readonly provider: string | null;
  readonly model: string | null;
  readonly capability_authority: E5CapabilityAuthority | null;
  readonly ownership: string;
  readonly budget: {
    readonly cap_micro: string | null;
    readonly open_micro: string;
    readonly settled_micro: string;
    readonly uncertain_micro: string;
  } | null;
  readonly generation: number | null;
  readonly delivery_point: string | null;
  readonly handoff: string | null;
  readonly artifacts: readonly string[];
}

export interface E5AgentsView {
  readonly presets: readonly E5AgentPreset[];
  readonly roles: readonly {
    readonly role: string;
    readonly provider: string | null;
    readonly model: string | null;
    readonly inherited: boolean;
    readonly capability_authority: E5CapabilityAuthority | null;
  }[];
  readonly tree: readonly E5AgentTreeNode[];
  readonly concurrency: {
    readonly limit: number | null;
    readonly active: number;
    readonly shared_budget_micro: string | null;
  } | null;
}

export interface E5ContextSource {
  readonly id: string;
  readonly kind: string;
  readonly uri: string;
  readonly range: string | null;
  readonly version: string | null;
  readonly content_hash: string | null;
  readonly bytes: number | null;
  readonly worktree: string | null;
  readonly inclusion_reason: string;
  readonly included: boolean;
  readonly pinned: boolean;
  readonly excluded: boolean;
  readonly failed_reason: string | null;
}

export interface E5ContextUsage {
  readonly capacity: number;
  readonly used: number;
  readonly output_reserve: number;
  readonly safety_reserve: number;
  readonly headroom: number;
  readonly basis: 'observed' | 'estimated';
  readonly categories: readonly { readonly category: string; readonly tokens: number; readonly basis: string }[];
}

export interface E5ContextView {
  readonly usage: E5ContextUsage | null;
  readonly manifest: {
    readonly revision: string;
    readonly sources: readonly E5ContextSource[];
    readonly captured_at_ms: number | null;
  } | null;
  readonly overflow: { readonly over_by: number; readonly actions: readonly string[] } | null;
  readonly retrieval: {
    readonly strategy: string;
    readonly embedding_provider: string | null;
    readonly embedding_model: string | null;
    readonly index_coverage: number | null;
    readonly index_fresh: boolean | null;
    readonly fallback: string | null;
  } | null;
  readonly editor_context: string;
  readonly ignore_patterns: readonly string[];
  readonly protected_patterns: readonly string[];
}

export interface E5CompactionView {
  readonly plan: {
    readonly expected_revision: string;
    readonly retained_facts: readonly string[];
    readonly summary_model: string;
    readonly egress: string;
    readonly cost_micro: string | null;
    readonly before_tokens: number | null;
    readonly minimum_reduction: number;
    readonly pending_operations: readonly string[];
    readonly candidate_identity: string | null;
  } | null;
  readonly operation: {
    readonly id: string;
    readonly state: string;
    readonly error: string | null;
    readonly measured_before: number | null;
    readonly measured_after: number | null;
    readonly summary_version: string | null;
    readonly minimum_reduction: number | null;
    readonly reduction_fraction: number | null;
  } | null;
  readonly threshold: number | null;
}

export interface E5InstructionSource {
  readonly id: string;
  readonly origin: string;
  readonly scope: string;
  readonly path: string | null;
  readonly enabled: boolean;
  readonly locked: boolean;
  readonly lock_source: string | null;
  readonly version: string | null;
  readonly hash: string | null;
  readonly content: string;
  readonly plugins: readonly string[];
}

export interface E5InstructionsView {
  readonly sources: readonly E5InstructionSource[];
  readonly effective: {
    readonly version: string;
    readonly hash: string;
    readonly merged: string;
    readonly order: readonly string[];
  } | null;
  readonly conflicts: readonly { readonly sources: readonly string[]; readonly detail: string }[];
  readonly templates: readonly { readonly name: string; readonly variables: readonly string[] }[];
}

export interface E5MemoryRecord {
  readonly id: string;
  readonly scope: string;
  readonly kind: string;
  readonly text: string;
  readonly source: string;
  readonly evidence_ref: string | null;
  readonly created_at_ms: number;
  readonly stopped_future_use_at_revision: string | null;
  readonly retained_prior_artifacts: readonly string[];
}

export interface E5MemoryView {
  readonly records: readonly E5MemoryRecord[];
  readonly proposal: string | null;
  readonly retention: { readonly policy: string; readonly dry_run: readonly string[] } | null;
}

export interface E5PolicyRule {
  readonly id: string;
  readonly preset: string;
  readonly tool: string;
  readonly action: string;
  readonly target: string;
  readonly decision: 'Allow' | 'Ask' | 'Deny';
  readonly scope: string;
  readonly expires_at_ms: number | null;
  readonly version: string;
  readonly source: string;
  readonly managed: boolean;
}

export interface E5PolicyView {
  readonly revision: string;
  readonly preset: string;
  readonly rules: readonly E5PolicyRule[];
  readonly filesystem: string;
  readonly shell: string;
  readonly network: string;
  readonly network_guarantee: string;
  readonly sandbox_state: string;
  readonly sandbox_error: string | null;
  readonly managed_denies: readonly string[];
  readonly injection_boundary: readonly string[];
  readonly audit: readonly {
    readonly at_ms: number;
    readonly request_id: string;
    readonly tool: string;
    readonly decision: string;
    readonly detail: string;
  }[];
  readonly simulation: {
    readonly decision: string;
    readonly matched_rule: string | null;
    readonly normalized: string | null;
    readonly effect_class: string | null;
    readonly reasons: readonly string[];
  } | null;
}

export interface E5ApprovalRequest {
  readonly request_id: string;
  readonly session: string;
  readonly owner: string;
  readonly child: string | null;
  readonly generation: number | null;
  readonly tool: string;
  readonly command: string | null;
  readonly cwd: string | null;
  readonly target: string | null;
  readonly effect_boundary: readonly string[];
  readonly arguments_preview: string;
  readonly env_names: readonly string[];
  readonly bounds: string | null;
  readonly created_at_ms: number;
  readonly expires_at_ms: number | null;
  readonly consumed: boolean;
  readonly expired: boolean;
  readonly remember_available: boolean;
}

export interface E5ApprovalsView {
  readonly pending: readonly E5ApprovalRequest[];
  readonly history: readonly {
    readonly request_id: string;
    readonly decision: string;
    readonly at_ms: number;
    readonly dispatched: boolean;
    readonly detail: string | null;
  }[];
}

export interface E5ToolEntry {
  readonly id: string;
  readonly origin: string;
  readonly server: string | null;
  readonly name: string;
  readonly effect_class: string;
  readonly discovered: boolean;
  readonly enabled: boolean;
  readonly healthy: boolean;
  readonly authorized: boolean;
  readonly permission: string;
  readonly schema: Json | null;
  readonly argument_preview: string | null;
}

export interface E5McpServer {
  readonly id: string;
  readonly name: string;
  readonly transport: string;
  readonly command: string;
  readonly args: readonly string[];
  readonly env_references: readonly string[];
  readonly source: string;
  readonly trusted: boolean;
  readonly health: string;
  readonly health_detail: string | null;
  readonly auth_state: string;
  readonly error: string | null;
  readonly stdout_cap: number | null;
  readonly stderr_cap: number | null;
  readonly timeout_ms: number | null;
  readonly tool_count: number;
  readonly logs: readonly string[];
  readonly resources: readonly string[];
  readonly prompts: readonly string[];
  readonly remote_supported: boolean;
}

export interface E5ToolsView {
  readonly builtin: readonly E5ToolEntry[];
  readonly mcp: readonly E5McpServer[];
  readonly tool_loading: string;
  readonly export_available: boolean;
}

export interface E5Skill {
  readonly id: string;
  readonly name: string;
  readonly version: string;
  readonly source: string;
  readonly trust: string;
  readonly enabled: boolean;
  readonly invocation: 'manual' | 'agent_selectable' | 'disabled';
  readonly capabilities: readonly string[];
  readonly files: readonly string[];
  readonly manifest: string;
  readonly version_diff: string | null;
  readonly dependencies_available: boolean;
  readonly prior_version: string | null;
}

export interface E5CommandEntry {
  readonly name: string;
  readonly scope: string;
  readonly effect: string;
  readonly template: string;
  readonly variables: readonly string[];
}

export interface E5Hook {
  readonly id: string;
  readonly event: string;
  readonly executable: string;
  readonly args: readonly string[];
  readonly match: string;
  readonly env_allowlist: readonly string[];
  readonly deadline_ms: number;
  readonly stdout_cap: number;
  readonly stderr_cap: number;
  readonly failure_policy: string;
  readonly version: string;
  readonly last_test: {
    readonly outcome: string;
    readonly exit_state: string | null;
    readonly verdict: string | null;
    readonly stdout: string;
    readonly stderr: string;
    readonly audit_ref: string | null;
    readonly spec_hash: string | null;
    readonly at_ms: number;
  } | null;
}

export interface E5SkillsView {
  readonly skills: readonly E5Skill[];
  readonly commands: readonly E5CommandEntry[];
  readonly hooks: readonly E5Hook[];
  readonly hook_events: readonly string[];
}

export interface E5TerminalProcess {
  readonly id: string;
  readonly pid: number | null;
  readonly owner: string;
  readonly binding: string;
  readonly live: boolean;
  readonly state: string;
  readonly cwd: string;
  readonly sandbox_profile: string;
  readonly command: string;
  readonly output_head: string;
  readonly output_tail: string;
  readonly stdout_bytes: number;
  readonly stderr_bytes: number;
  readonly artifact_ref: string | null;
  readonly effect_status: string | null;
  readonly handle_generation: number | null;
}

export interface E5TerminalView {
  readonly processes: readonly E5TerminalProcess[];
  readonly profiles: readonly { readonly name: string; readonly detail: string }[];
  readonly queue: readonly { readonly id: string; readonly text: string; readonly state: string }[];
}

export interface E5BrowserView {
  readonly adapters: readonly {
    readonly id: string;
    readonly name: string;
    readonly available: boolean;
    readonly unavailable_reason: string | null;
    readonly account: string | null;
    readonly machine: string | null;
  }[];
  readonly sessions: readonly {
    readonly id: string;
    readonly url: string;
    readonly account: string | null;
    readonly machine: string | null;
    readonly network_scope: string;
    readonly screenshot_ref: string | null;
    readonly console: readonly string[];
    readonly network: readonly string[];
  }[];
}

export interface E5BudgetView {
  readonly token_cap: number | null;
  readonly cost_cap_micro: string | null;
  readonly attempts: number | null;
  readonly base_delay_ms: number | null;
  readonly max_delay_ms: number | null;
  readonly jitter: number | null;
  readonly deadline_ms: number | null;
  readonly loop_limits: { readonly steps: number | null; readonly consecutive_failures: number | null } | null;
  readonly ledger: {
    readonly open_micro: string;
    readonly settled_micro: string;
    readonly refunded_micro: string;
    readonly uncertain_micro: string;
  } | null;
  readonly warning: string | null;
  readonly enforced: boolean;
  readonly rate_limit: {
    readonly cause: string;
    readonly retry_at_ms: number | null;
    readonly attempt: number;
    readonly max_attempts: number;
  } | null;
  readonly revision: string | null;
}

export interface E5RunView {
  readonly state: string;
  readonly timeline: readonly {
    readonly id: string;
    readonly kind: string;
    readonly state: string;
    readonly detail: string;
    readonly at_ms: number;
    readonly effect_status: string | null;
    readonly call: E5CallIdentity | null;
  }[];
  readonly queue: readonly { readonly id: string; readonly text: string; readonly state: string }[];
  readonly display: string;
  readonly attention: readonly string[];
  readonly stale: boolean;
}

export interface E5ChangeView {
  readonly candidate: string | null;
  readonly base: string | null;
  readonly files: readonly {
    readonly path: string;
    readonly status: string;
    readonly additions: number;
    readonly deletions: number;
    readonly hunks: readonly string[];
  }[];
  readonly comments: readonly {
    readonly id: string;
    readonly path: string;
    readonly line: number;
    readonly body: string;
    readonly version: string;
  }[];
  readonly acceptance: readonly string[];
  readonly manual_edit_conflict: string | null;
}

export interface E5Checkpoint {
  readonly id: string;
  readonly created_at_ms: number;
  readonly before_hash: string | null;
  readonly after_hash: string | null;
  readonly content_available: boolean;
  readonly coverage: string;
  readonly uncovered_effects: readonly string[];
  readonly redo_supported: boolean;
  readonly diff_supported: boolean;
  readonly files: readonly string[];
  readonly conversation_range: string | null;
}

export interface E5HistorySession {
  readonly id: string;
  readonly title: string;
  readonly provider: string | null;
  readonly model: string | null;
  readonly state: string;
  readonly worktree: string | null;
  readonly outcome: string | null;
  readonly pinned: boolean;
  readonly archived: boolean;
  readonly updated_at_ms: number;
  readonly active_run: boolean;
}

export interface E5HistoryView {
  readonly sessions: readonly E5HistorySession[];
  readonly next_cursor: string | null;
  readonly total: number | null;
  readonly search: {
    readonly query: string;
    readonly mode: string;
    readonly results: readonly { readonly session: string; readonly seq: number; readonly excerpt: string }[];
    readonly next_cursor: string | null;
  } | null;
  readonly archived_is_deleted: false;
}

export interface E5UsageView {
  readonly attempted_spend_micro: string;
  readonly verified_landed_changes: number | null;
  readonly vcsc_micro: string | null;
  readonly vcsc_state: string;
  readonly cohort: string;
  readonly period: string;
  readonly denominator: string;
  readonly tokens_in: number | null;
  readonly tokens_out: number | null;
  readonly cache_read: number | null;
  readonly cache_write: number | null;
  readonly provider_quota: string | null;
  readonly local_estimate: string | null;
  readonly observed_charge: string | null;
}

export interface E5OutcomesView {
  readonly verified: number;
  readonly unverified: number;
  readonly pending: number;
  readonly receipts: readonly {
    readonly id: string;
    readonly candidate: string;
    readonly verified_at_ms: number;
    readonly landed: boolean;
    readonly detail: string;
  }[];
}

export interface E5EfficiencyView {
  readonly flags: readonly {
    readonly key: string;
    readonly enabled: boolean;
    readonly provenance: string;
    readonly measured: string | null;
    readonly sample_count: number | null;
  }[];
  readonly retrieval: string;
  readonly fallback: string | null;
  readonly aliases: readonly string[];
}

export interface E5SettingsView {
  readonly search: readonly {
    readonly key: string;
    readonly effective: Json;
    readonly source: string;
    readonly locked: boolean;
    readonly scope: string;
  }[];
  readonly import_export: {
    readonly export_available: boolean;
    readonly import_available: boolean;
    readonly schema: string;
  } | null;
  readonly telemetry: { readonly categories: readonly string[]; readonly enabled: boolean } | null;
}

export interface E5DiagnosticsView {
  readonly readiness: string;
  readonly readiness_detail: string | null;
  readonly version: string | null;
  readonly runtime_path: string | null;
  readonly checksum: string | null;
  readonly checksum_verified: boolean;
  readonly signature_verified: boolean;
  readonly trust: string;
  readonly protocol_version: number | null;
  readonly protocol_compatible: boolean;
  readonly connectivity: {
    readonly state: string;
    readonly detail: string | null;
    readonly retryable: boolean;
  } | null;
  readonly uncertain_admissions: readonly {
    readonly operation_id: string;
    readonly receipt: string | null;
    readonly retry_eligible: boolean;
    readonly effect_status: string;
  }[];
  readonly export_preview: {
    readonly categories: readonly string[];
    readonly omitted: readonly string[];
    readonly bytes: number;
    readonly redacted: true;
  } | null;
  readonly log_tail: readonly string[];
}

export interface E5BindingView {
  readonly editor_host: string | null;
  readonly workspace_uri: string | null;
  readonly daemon_host: string | null;
  readonly model_endpoint: string | null;
  readonly embedding_endpoint: string | null;
  readonly vault_location: string | null;
  readonly worker_location: string | null;
  readonly data_boundary: string;
  readonly generation: number | null;
  readonly stale_handles: readonly string[];
}

export interface E5ProofView {
  readonly outcome: 'verified' | 'unverified' | 'pending' | 'unavailable';
  readonly required_total: number;
  readonly required_passed: number;
  readonly criteria: readonly {
    readonly key: string;
    readonly requirement: string;
    readonly verdict: 'pass' | 'fail' | 'unavailable';
    readonly binding: string;
    readonly evidence_refs: readonly string[];
    readonly detail: string;
  }[];
  readonly claims_complete: boolean | null;
  readonly candidate: string | null;
  readonly verified_candidate: string | null;
  readonly landed_candidate: string | null;
  readonly finish_requested: string | null;
  readonly finish_receipt: string | null;
  readonly blockers: readonly string[];
  readonly receipts: readonly { readonly id: string; readonly label: string; readonly kind: string }[];
}

export interface E5Snapshot {
  readonly schema: 'faktor-edition5-ui/v1';
  readonly revision: string;
  readonly generated_at_ms: number;
  readonly stale: boolean;
  readonly stale_age_ms: number | null;
  readonly binding: E5BindingTuple | null;
  readonly config: E5ConfigSnapshot | null;
  readonly models: E5ModelsView | null;
  readonly providers: E5ProvidersView | null;
  readonly generation: E5GenerationView | null;
  readonly agents: E5AgentsView | null;
  readonly context: E5ContextView | null;
  readonly compaction: E5CompactionView | null;
  readonly instructions: E5InstructionsView | null;
  readonly memory: E5MemoryView | null;
  readonly policy: E5PolicyView | null;
  readonly approvals: E5ApprovalsView | null;
  readonly tools: E5ToolsView | null;
  readonly skills: E5SkillsView | null;
  readonly terminal: E5TerminalView | null;
  readonly browser: E5BrowserView | null;
  readonly budget: E5BudgetView | null;
  readonly run: E5RunView | null;
  readonly changes: E5ChangeView | null;
  readonly checkpoints: readonly E5Checkpoint[] | null;
  readonly history: E5HistoryView | null;
  readonly usage: E5UsageView | null;
  readonly outcomes: E5OutcomesView | null;
  readonly efficiency: E5EfficiencyView | null;
  readonly settings: E5SettingsView | null;
  readonly diagnostics: E5DiagnosticsView | null;
  readonly binding_view: E5BindingView | null;
  readonly proof: E5ProofView | null;
}

// ------------------------------------------------------------- command DTOs

export interface E5ConfigPatchResult {
  readonly revision: string;
  readonly applied: boolean;
  readonly field_states: readonly { readonly key: string; readonly state: E5ApplyState }[];
  readonly errors: readonly { readonly key: string; readonly code: string; readonly message: string }[];
  readonly config: E5ConfigSnapshot | null;
}

export interface E5ProviderTestResult {
  readonly stages: readonly { readonly stage: string; readonly status: string; readonly detail: string | null }[];
  readonly observed_at_ms: number;
  readonly credential_verified: boolean;
  readonly discovery: readonly string[];
  readonly failure: string | null;
}

export interface E5ConfigPatch {
  readonly expected_revision: string;
  readonly set: readonly { readonly key: string; readonly value: Json; readonly control_id: string }[];
  readonly unset: readonly string[];
}

export interface E5ProviderDraft {
  readonly id: string | null;
  readonly label: string;
  readonly kind: string;
  readonly base_url: string;
  readonly credential_reference: string | null;
  readonly headers: readonly { readonly name: string; readonly reference: string }[];
  readonly allow_loopback: boolean;
  readonly transport_timeout_ms: number | null;
  readonly proxy: string | null;
  readonly tls: string | null;
  readonly models: readonly string[];
  readonly expected_revision: string | null;
}

export interface E5McpDraft {
  readonly id: string | null;
  readonly name: string;
  readonly transport: string;
  readonly command: string;
  readonly args: readonly string[];
  readonly env_references: readonly string[];
  readonly trusted: boolean;
  readonly source: string;
  readonly timeout_ms: number | null;
  readonly stdout_cap: number | null;
  readonly stderr_cap: number | null;
}

export interface E5HookDraft {
  readonly id: string | null;
  readonly event: string;
  readonly executable: string;
  readonly args: readonly string[];
  readonly match: string;
  readonly env_allowlist: readonly string[];
  readonly deadline_ms: number;
  readonly stdout_cap: number;
  readonly stderr_cap: number;
  readonly failure_policy: string;
}

export interface E5CompactionResult {
  readonly operation_id: string;
  readonly state: string;
  readonly measured_before: number | null;
  readonly measured_after: number | null;
  readonly reduction_fraction: number | null;
  readonly summary_version: string | null;
  readonly error: string | null;
  readonly prior_basis_preserved: boolean;
}

export interface E5RestorePreview {
  readonly checkpoint: string;
  readonly scope: string;
  readonly files: readonly string[];
  readonly conversation_range: string | null;
  readonly uncovered_effects: readonly string[];
  readonly content_available: boolean;
  readonly expected_current_hash: string | null;
  readonly conflict: string | null;
}
