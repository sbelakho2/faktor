// Every Edition 5 control maps to one typed host command. This is the single
// registry the UI renders and the exercise suite drives: if a control has no
// binding, `assertBindingsComplete` fails the build/selftest. Config-backed
// controls stage one typed value (atomic apply); action controls invoke the
// named backend method; read-only controls perform a served re-read.

import { E5_CONTROLS, type Edition5Control } from '../generated/edition5Registry.ts';

export type E5CommandKind =
  | 'inspect'
  | 'config.set'
  | 'provider.create'
  | 'provider.update'
  | 'provider.delete'
  | 'provider.test'
  | 'provider.rotate'
  | 'provider.discover'
  | 'catalog.refresh'
  | 'catalog.probe'
  | 'model.select'
  | 'model.favorite'
  | 'generation.preview'
  | 'generation.reset'
  | 'preset.select'
  | 'preset.save'
  | 'preset.duplicate'
  | 'role.save'
  | 'context.setEditorContext'
  | 'context.add'
  | 'context.remove'
  | 'context.retry'
  | 'context.pin'
  | 'context.exclude'
  | 'context.export'
  | 'context.preview'
  | 'context.reindex'
  | 'compaction.preview'
  | 'compaction.run'
  | 'compaction.cancel'
  | 'instruction.save'
  | 'instruction.toggle'
  | 'instruction.preview'
  | 'instruction.import'
  | 'instruction.test'
  | 'memory.save'
  | 'memory.delete'
  | 'memory.review'
  | 'policy.preview'
  | 'policy.apply'
  | 'policy.revoke'
  | 'policy.audit'
  | 'approval.reply'
  | 'approval.remember'
  | 'approval.refresh'
  | 'mcp.create'
  | 'mcp.start'
  | 'mcp.stop'
  | 'mcp.restart'
  | 'mcp.test'
  | 'mcp.remove'
  | 'mcp.auth'
  | 'tool.enable'
  | 'tool.permissions'
  | 'tool.logs'
  | 'tool.export'
  | 'skill.install'
  | 'skill.update'
  | 'skill.remove'
  | 'skill.enable'
  | 'skill.invoke'
  | 'command.insert'
  | 'hook.save'
  | 'hook.test'
  | 'terminal.background'
  | 'terminal.stop'
  | 'terminal.input'
  | 'terminal.reconcile'
  | 'browser.connect'
  | 'browser.capture'
  | 'browser.diagnostics'
  | 'budget.revise'
  | 'retry.cancel'
  | 'queue.edit'
  | 'queue.remove'
  | 'queue.next'
  | 'steer.send'
  | 'agent.pause'
  | 'agent.stop'
  | 'agent.model'
  | 'change.review'
  | 'change.comment'
  | 'change.hunkAccept'
  | 'change.followup'
  | 'restore.preview'
  | 'restore.run'
  | 'fork.run'
  | 'redo.run'
  | 'session.rename'
  | 'session.pin'
  | 'session.archive'
  | 'session.delete'
  | 'session.export'
  | 'session.fork'
  | 'session.switch'
  | 'search.titles'
  | 'search.transcript'
  | 'usage.filter'
  | 'routing.preview'
  | 'routing.save'
  | 'settings.apply'
  | 'settings.search'
  | 'settings.import'
  | 'settings.export'
  | 'diagnostics.export'
  | 'diagnostics.preview'
  | 'diagnostics.lifecycle'
  | 'binding.revalidate'
  | 'binding.rebind'
  | 'finish.request'
  | 'host.action';

export type E5InputKind =
  | 'toggle'
  | 'select'
  | 'number'
  | 'text'
  | 'textarea'
  | 'secret-ref'
  | 'list'
  | 'readonly'
  | 'action'
  | 'money';

export type E5OptionsSource =
  | 'models'
  | 'providers'
  | 'presets'
  | 'roles'
  | 'hooks'
  | 'skills'
  | 'mcp'
  | 'tools'
  | 'processes'
  | 'checkpoints'
  | null;

export interface E5Binding {
  readonly controlId: string;
  readonly command: E5CommandKind;
  readonly configKey: string | null;
  readonly input: E5InputKind;
  readonly options: readonly string[];
  readonly optionsSource: E5OptionsSource;
  readonly actionLabel: string;
  readonly placeholder: string | null;
  readonly min: number | null;
  readonly max: number | null;
  readonly step: number | null;
  readonly unit: string | null;
  /** Representative staged value the exercise suite uses for this control. */
  readonly sample: string | number | boolean;
}

const CONFIG_KEYS: Readonly<Record<string, string>> = {
  'M-01': 'faktor.defaultProvider',
  'M-02': 'faktor.defaultModel',
  'M-04': 'routing_mode',
  'M-09': 'model.contextLimit',
  'M-10': 'provider.quality',
  'P-01': 'providers[].id',
  'P-02': 'providers[].kind',
  'P-03': 'providers[].base_url',
  'P-04': 'providers[].api_key_env',
  'P-05': 'providers[].headers',
  'P-06': 'providers[].models',
  'P-07': 'providers[].kind',
  'P-08': 'providers[].allow_loopback',
  'P-09': 'sandbox.network',
  'P-11': 'provider.transport_timeout_ms',
  'P-15': 'ollama.keep_alive',
  'G-01': 'model.reasoning',
  'G-02': 'model.thinking_budget',
  'G-03': 'task.max_tokens',
  'G-04': 'temperature',
  'G-05': 'top_p',
  'G-06': 'top_k',
  'G-07': 'frequency_penalty',
  'G-08': 'seed',
  'G-09': 'stop',
  'G-10': 'parallel_tool_calls',
  'G-11': 'output_format',
  'G-12': 'service_tier',
  'G-13': 'streaming',
  'C-08': 'context.ignore_patterns',
  'C-09': 'retrieval.strategy',
  'C-10': 'retrieval.embedding',
  'K-01': 'compact_at_usage',
  'K-02': 'compaction_model',
  'K-05': 'efficiency.failure_learning',
  'K-06': 'efficiency.ccr',
  'K-07': 'efficiency.typed_handoff',
  'K-08': 'efficiency.semantic_context',
  'K-09': 'efficiency.rework_routing',
  'K-10': 'memory.capture',
  'I-01': 'instructions',
  'I-02': 'AGENTS.md',
  'I-03': '.faktor/rules',
  'I-06': 'profiles.commands',
  'I-10': 'output.style',
  'Q-01': 'permission.preset',
  'Q-02': 'permission.rules',
  'Q-03': 'sandbox.filesystem',
  'Q-04': 'sandbox.shell',
  'Q-05': 'sandbox.network',
  'Q-10': 'sandbox.network_guarantee',
  'Q-12': 'enterprise.settings',
  'T-03': 'mcp',
  'T-04': 'mcp[].transport',
  'T-05': 'mcp[].env',
  'T-06': 'mcp[].oauth',
  'T-08': 'permission.rules',
  'T-10': 'tool.bounds',
  'S-07': 'hooks[].events',
  'S-08': 'hooks[].command',
  'S-10': 'hooks[].env_allowlist',
  'S-11': 'hooks[].deadline_ms',
  'S-12': 'hooks[].failure_policy',
  'S-13': 'hooks[].test',
  'X-02': 'sandbox.shell',
  'X-12': 'sandbox.shell',
  'B-01': 'faktor.budgetTokens',
  'B-02': 'faktor.budgetCostMicro',
  'B-04': 'usage.durable.reservations',
  'B-05': 'task.budget_revision',
  'B-06': 'retry.max_attempts',
  'B-07': 'retry.base_delay_ms',
  'B-09': 'agent.loop_bounds',
  'B-10': 'verification.quick_max_s',
  'V-07': 'completion_contract',
  'V-08': 'verification.policy',
  'V-09': 'completion.proof',
  'V-10': 'completion.level',
  'V-11': 'completion.scm',
  'V-12': 'expected_current_hash',
  'D-04': 'session.metadata',
  'D-05': 'prompt.queue',
  'D-06': 'session.export_delete',
  'D-08': 'display.mode',
  'D-09': 'density',
  'D-12': 'notifications.categories',
  'D-10': 'send.keybinding',
  'D-11': 'commands.help',
  'O-01': 'settings.search',
  'O-02': 'route.egress_policy',
  'O-03': 'diagnostic.export',
  'O-04': 'faktor.binaryPath',
  'O-05': 'updater',
  'O-06': 'billing',
  'O-07': 'faktor.controlPlaneEndpoint',
  'O-08': 'enterprise',
  'O-09': 'retention.policy',
  'O-10': 'telemetry',
  'O-11': 'workers',
  'O-12': 'config.import_export',
  'O-13': 'runtime.capability_handshake',
};

const COMMANDS: Readonly<Record<string, E5CommandKind>> = {
  'M-01': 'model.select',
  'M-02': 'model.select',
  'M-03': 'model.favorite',
  'M-05': 'role.save',
  'M-06': 'routing.save',
  'M-08': 'catalog.probe',
  'M-12': 'model.select',
  'M-14': 'catalog.probe',
  'P-10': 'provider.test',
  'P-13': 'inspect',
  'P-14': 'provider.rotate',
  'G-02': 'generation.preview',
  'G-04': 'generation.preview',
  'G-05': 'generation.preview',
  'G-06': 'generation.preview',
  'G-07': 'generation.preview',
  'G-08': 'generation.preview',
  'G-09': 'generation.preview',
  'G-10': 'generation.preview',
  'G-11': 'generation.preview',
  'G-12': 'generation.preview',
  'G-14': 'inspect',
  'A-01': 'preset.select',
  'A-02': 'inspect',
  'A-03': 'inspect',
  'A-04': 'preset.save',
  'A-05': 'preset.save',
  'A-06': 'role.save',
  'A-07': 'preset.save',
  'A-08': 'preset.save',
  'A-09': 'preset.save',
  'A-10': 'agent.pause',
  'A-11': 'preset.save',
  'A-12': 'preset.save',
  'C-01': 'inspect',
  'C-02': 'inspect',
  'C-03': 'context.add',
  'C-04': 'context.setEditorContext',
  'C-05': 'context.add',
  'C-06': 'context.add',
  'C-07': 'context.pin',
  'C-08': 'config.set',
  'C-09': 'config.set',
  'C-10': 'config.set',
  'C-11': 'context.reindex',
  'C-12': 'context.preview',
  'C-13': 'inspect',
  'C-14': 'context.export',
  'K-03': 'compaction.preview',
  'K-04': 'inspect',
  'K-10': 'config.set',
  'K-11': 'memory.delete',
  'K-12': 'inspect',
  'I-04': 'inspect',
  'I-05': 'instruction.toggle',
  'I-07': 'inspect',
  'I-08': 'instruction.import',
  'I-09': 'inspect',
  'I-10': 'config.set',
  'I-11': 'inspect',
  'Q-06': 'approval.reply',
  'Q-07': 'approval.remember',
  'Q-08': 'policy.preview',
  'Q-09': 'policy.revoke',
  'Q-11': 'host.action',
  'Q-13': 'policy.audit',
  'Q-14': 'inspect',
  'T-01': 'inspect',
  'T-02': 'tool.enable',
  'T-07': 'inspect',
  'T-09': 'inspect',
  'T-11': 'inspect',
  'T-12': 'inspect',
  'T-13': 'mcp.restart',
  'T-14': 'mcp.create',
  'T-15': 'tool.export',
  'S-01': 'inspect',
  'S-02': 'skill.enable',
  'S-03': 'inspect',
  'S-04': 'skill.install',
  'S-05': 'command.insert',
  'S-06': 'instruction.save',
  'S-09': 'hook.save',
  'S-13': 'hook.test',
  'S-14': 'inspect',
  'X-01': 'inspect',
  'X-03': 'inspect',
  'X-04': 'terminal.background',
  'X-05': 'terminal.stop',
  'X-06': 'browser.connect',
  'X-07': 'browser.connect',
  'X-08': 'browser.capture',
  'X-09': 'browser.diagnostics',
  'X-10': 'context.add',
  'X-11': 'context.add',
  'X-13': 'inspect',
  'B-03': 'inspect',
  'B-08': 'retry.cancel',
  'B-11': 'diagnostics.lifecycle',
  'B-12': 'diagnostics.preview',
  'V-01': 'change.review',
  'V-02': 'change.comment',
  'V-03': 'change.hunkAccept',
  'V-04': 'inspect',
  'V-05': 'restore.preview',
  'V-06': 'fork.run',
  'D-01': 'session.switch',
  'D-02': 'search.titles',
  'D-03': 'search.transcript',
  'D-04': 'session.rename',
  'D-05': 'queue.edit',
  'D-06': 'session.export',
  'D-07': 'inspect',
  'D-08': 'config.set',
  'D-11': 'host.action',
  'D-12': 'config.set',
  'D-13': 'inspect',
  'D-14': 'host.action',
  'D-15': 'session.switch',
  'O-01': 'settings.search',
  'O-02': 'inspect',
  'O-03': 'diagnostics.export',
  'O-04': 'inspect',
  'O-05': 'diagnostics.lifecycle',
  'O-06': 'inspect',
  'O-07': 'host.action',
  'O-08': 'inspect',
  'O-10': 'config.set',
  'O-11': 'config.set',
  'O-12': 'settings.import',
  'O-13': 'inspect',
};

const INPUTS: Readonly<Record<string, E5InputKind>> = {
  'M-01': 'select',
  'M-02': 'select',
  'M-04': 'select',
  'M-09': 'number',
  'M-10': 'number',
  'P-02': 'select',
  'P-03': 'text',
  'P-04': 'secret-ref',
  'P-05': 'list',
  'P-06': 'list',
  'P-07': 'select',
  'P-08': 'toggle',
  'P-09': 'text',
  'P-11': 'number',
  'P-15': 'select',
  'G-01': 'select',
  'G-02': 'number',
  'G-03': 'number',
  'G-04': 'number',
  'G-05': 'number',
  'G-06': 'number',
  'G-07': 'number',
  'G-08': 'number',
  'G-09': 'list',
  'G-10': 'toggle',
  'G-11': 'select',
  'G-12': 'select',
  'G-13': 'toggle',
  'K-01': 'number',
  'K-02': 'select',
  'K-05': 'toggle',
  'K-06': 'toggle',
  'K-07': 'toggle',
  'K-08': 'toggle',
  'K-09': 'toggle',
  'K-10': 'select',
  'I-01': 'textarea',
  'I-02': 'textarea',
  'I-03': 'textarea',
  'I-06': 'textarea',
  'I-10': 'select',
  'Q-01': 'select',
  'Q-02': 'select',
  'Q-03': 'text',
  'Q-04': 'text',
  'Q-05': 'text',
  'Q-10': 'select',
  'Q-12': 'readonly',
  'T-02': 'toggle',
  'T-04': 'select',
  'T-05': 'list',
  'T-10': 'number',
  'S-07': 'select',
  'S-08': 'list',
  'S-10': 'list',
  'S-11': 'number',
  'S-12': 'select',
  'S-13': 'action',
  'X-02': 'select',
  'B-01': 'number',
  'B-02': 'money',
  'B-05': 'number',
  'B-06': 'number',
  'B-07': 'number',
  'B-09': 'number',
  'B-10': 'number',
  'V-07': 'textarea',
  'V-08': 'select',
  'V-09': 'readonly',
  'V-10': 'select',
  'V-11': 'text',
  'V-12': 'readonly',
  'D-08': 'select',
  'D-09': 'select',
  'D-10': 'select',
  'D-12': 'list',
  'O-10': 'toggle',
  'O-13': 'readonly',
};

const OPTIONS: Readonly<Record<string, readonly string[]>> = {
  'M-04': ['pinned', 'economy'],
  'P-02': ['openai_chat', 'openai_responses', 'anthropic', 'google', 'deepseek', 'ollama', 'gateway'],
  'P-07': ['ollama', 'ollama_compatible'],
  'P-15': ['5m', '15m', '30m', '-1'],
  'G-01': ['off', 'low', 'medium', 'high'],
  'G-11': ['text', 'json_schema'],
  'G-12': ['default', 'flex', 'priority'],
  'K-02': ['inherit', 'economic', 'pinned'],
  'K-05': ['on', 'off'],
  'K-10': ['off', 'manual', 'controlled_auto'],
  'I-10': ['concise', 'explanatory', 'custom'],
  'Q-01': ['inspect', 'confirm_edits', 'project_auto', 'custom'],
  'Q-02': ['Allow', 'Ask', 'Deny'],
  'Q-10': ['strict', 'network_fallback', 'unavailable'],
  'T-04': ['stdio'],
  'S-07': [
    'SessionStart',
    'UserPromptSubmit',
    'PreToolUse',
    'PostToolUse',
    'PostToolUseFailure',
    'PermissionRequest',
    'Notification',
    'Stop',
    'SubagentStart',
    'SubagentStop',
    'PreCompact',
    'PostCompact',
    'SessionEnd',
    'PreModelCall',
    'PostModelCall',
    'PreEdit',
    'PostEdit',
  ],
  'S-12': ['FailClosed', 'FailOpen', 'Warn'],
  'X-02': ['sandbox', 'host'],
  'D-08': ['compact', 'balanced', 'detailed'],
  'D-09': ['inherit', 'comfortable', 'compact'],
  'D-10': ['enter_sends', 'mod_enter_sends'],
  'V-08': ['quick', 'unit', 'full'],
  'V-10': ['local', 'commit', 'push', 'pr'],
};

const OPTIONS_SOURCE: Readonly<Record<string, E5OptionsSource>> = {
  'M-01': 'providers',
  'M-02': 'models',
  'M-05': 'models',
  'K-02': 'models',
  'A-01': 'presets',
  'T-06': 'mcp',
  'S-02': 'skills',
  'D-01': 'models',
};

const ACTION_LABELS: Readonly<Record<string, string>> = {
  'M-03': 'Favorite selected model',
  'M-08': 'Probe exact model',
  'M-12': 'Apply to next task',
  'M-14': 'Start comparison',
  'P-10': 'Test connection',
  'P-13': 'Inspect binding',
  'P-14': 'Rotate credential',
  'G-02': 'Preview bundle',
  'G-04': 'Preview bundle',
  'G-05': 'Preview bundle',
  'G-06': 'Preview bundle',
  'G-07': 'Preview bundle',
  'G-08': 'Preview bundle',
  'G-09': 'Preview bundle',
  'G-10': 'Preview bundle',
  'G-11': 'Preview bundle',
  'G-12': 'Preview bundle',
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
  'I-05': 'Toggle rule',
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
  'S-09': 'Preview match',
  'S-13': 'Run bounded test',
  'X-04': 'Run in background',
  'X-05': 'Stop owned process',
  'X-06': 'Connect browser',
  'X-08': 'Capture artifact',
  'X-09': 'Open issues table',
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
};

const SAMPLES: Readonly<Record<E5InputKind, string | number | boolean>> = {
  toggle: true,
  select: '',
  number: 1,
  text: 'value',
  textarea: 'value',
  'secret-ref': 'env:FAKTOR_SAMPLE',
  list: 'value',
  readonly: '',
  action: '',
  money: '1',
};

function configKeyFor(control: Edition5Control): string | null {
  if (CONFIG_KEYS[control.id] !== undefined) return CONFIG_KEYS[control.id];
  if (control.configKeys.length > 0) {
    return control.configKeys[0].replace(' (proposed)', '').replace(' (per-instance)', '').trim();
  }
  return null;
}

export function bindingFor(control: Edition5Control): E5Binding {
  const configKey = configKeyFor(control);
  const explicit = COMMANDS[control.id];
  const command: E5CommandKind =
    explicit ?? (configKey !== null ? 'config.set' : control.classification === 'H' ? 'host.action' : 'inspect');
  const input: E5InputKind =
    INPUTS[control.id] ??
    (command === 'config.set'
      ? 'text'
      : command === 'inspect'
        ? 'readonly'
        : 'action');
  const options = OPTIONS[control.id] ?? [];
  return {
    controlId: control.id,
    command,
    configKey,
    input,
    options,
    optionsSource: OPTIONS_SOURCE[control.id] ?? null,
    actionLabel:
      ACTION_LABELS[control.id] ??
      (command === 'inspect' ? 'Inspect served state' : command === 'config.set' ? 'Stage value' : 'Apply'),
    placeholder: input === 'text' || input === 'textarea' || input === 'secret-ref' ? 'Enter a value' : null,
    min: input === 'number' ? 0 : null,
    max: null,
    step: input === 'number' ? 1 : null,
    unit: null,
    sample: options.length > 0 ? options[0] : SAMPLES[input],
  };
}

export const E5_BINDINGS: ReadonlyMap<string, E5Binding> = new Map(
  E5_CONTROLS.map((control) => [control.id, bindingFor(control)]),
);

export function bindingOf(controlId: string): E5Binding {
  const binding = E5_BINDINGS.get(controlId);
  if (binding === undefined) {
    throw new Error(`edition5: no binding for control ${controlId}`);
  }
  return binding;
}

/** Build/test gate: every control must resolve to a binding and a command. */
export function assertBindingsComplete(): readonly string[] {
  const missing: string[] = [];
  for (const control of E5_CONTROLS) {
    const binding = E5_BINDINGS.get(control.id);
    if (binding === undefined || binding.command.length === 0) {
      missing.push(control.id);
    }
  }
  return missing;
}

/** A representative command for a control's input kind. */
export function sampleValue(binding: E5Binding, options: readonly string[] = []): string | number | boolean {
  if (binding.input === 'select') {
    return options.length > 0 ? options[0] : binding.sample;
  }
  return binding.sample;
}


/** Generation control -> typed parameter key in the preview/apply DTO. */
export const GENERATION_PARAMETER_KEYS: Readonly<Record<string, string>> = {
  'G-01': 'reasoning',
  'G-02': 'thinking_budget',
  'G-03': 'max_output',
  'G-04': 'temperature',
  'G-05': 'top_p',
  'G-06': 'top_k',
  'G-07': 'frequency_penalty',
  'G-08': 'seed',
  'G-09': 'stop',
  'G-10': 'parallel_tool_calls',
  'G-11': 'output_format',
  'G-12': 'service_tier',
  'G-13': 'stream',
};
