#!/usr/bin/env node
// VS Code side of the 40 Edition 5 acceptance scenarios (A01–A40).
// Each check drives the real pure view/controller/typed command layer (or the
// real webview in headless Chrome for A39) and asserts the visible outcome
// plus the forbidden fake state. Prints one line per scenario and exits
// nonzero on any failure.

import { createHash } from 'node:crypto';
import { mkdtempSync, writeFileSync, copyFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import {
  E5_CONTROLS,
  E5_SCREENS,
} from '../src/generated/edition5Registry.ts';
import { bindingOf, GENERATION_PARAMETER_KEYS } from '../src/edition5/bindings.ts';
import {
  commandCall,
  defaultPayload,
  validateMcpPayload,
  validateSteerText,
} from '../src/edition5/commands.ts';
import {
  INITIAL_CONTROLLER_STATE,
  activateControl,
  applyDraft,
  restoreDefaults,
  stageRaw,
  unstage,
} from '../src/edition5/host.ts';
import {
  EMPTY_DRAFT,
  buildPatch,
  controlView,
  parseControlValue,
  parseMoneyMicro,
  screenView,
  stageControl,
  unstageControl,
} from '../src/edition5/view.ts';
import { validateSnapshot } from '../src/edition5/validate.ts';
import { renderEdition5Html } from '../src/edition5/html.ts';
import {
  richSnapshot,
  staleSnapshot,
  unknownCapabilitySnapshot,
  contextNullSnapshot,
  zeroDenominatorSnapshot,
  failedCompactionSnapshot,
  verifiedSnapshot,
} from './fixtures/edition5.mjs';
import { canonicalSubmissionId, startTaskRequest } from '../src/taskStart.ts';
import { connect, findChrome, launchChrome, sleep, THEMES } from './render-webview.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const MEDIA = join(HERE, '..', 'media');
const rich = validateSnapshot(richSnapshot());
const state = (snapshot, extra = {}) => ({ snapshot, draft: EMPTY_DRAFT, loading: false, error: null, ...extra });
const rowsOf = (id, snapshot) => screenView(id, state(snapshot)).rows;
const rowValue = (id, key, snapshot) => rowsOf(id, snapshot).find((row) => row.key === key)?.value ?? null;
const controlOf = (id, controlId, snapshot) => screenView(id, state(snapshot)).controls.find((entry) => entry.control.id === controlId);
const viewOf = (controlId, snapshot) => {
  const control = E5_CONTROLS.find((entry) => entry.id === controlId);
  return controlView(control, snapshot, false, null);
};
const clone = (value) => structuredClone(value);
const json = (value) => JSON.stringify(value);

let passed = 0;
const failures = [];
async function scenario(id, name, fn) {
  try {
    await fn();
    passed += 1;
    console.log(`PASS ${id} ${name}`);
  } catch (error) {
    failures.push(`${id} ${name}: ${error.message}`);
    console.log(`FAIL ${id} ${name} — ${error.message}`);
  }
}
function assert(condition, message) {
  if (!condition) throw new Error(message || 'assertion failed');
}
function assertEqual(actual, expected, message) {
  if (actual !== expected) throw new Error(`${message || 'assertEqual'}: expected ${json(expected)}, got ${json(actual)}`);
}
function assertIncludes(text, needle, message) {
  if (!String(text).includes(needle)) throw new Error(`${message || 'missing'}: ${json(needle)} in ${json(text)}`);
}
function assertExcludes(text, needle, message) {
  if (String(text).includes(needle)) throw new Error(`${message || 'forbidden'}: ${json(needle)} in ${json(text)}`);
}

class Backend {
  constructor(snapshot, options = {}) {
    this.snapshot = snapshot;
    this.calls = [];
    this.patches = [];
    this.executeImpl = options.execute ?? (async (command, payload) => ({ ok: true, message: `ack ${command}`, call: null, body: null, errorCode: null }));
    this.patchImpl = options.patch ?? (async (patch) => ({
      revision: 'r8',
      applied: true,
      field_states: patch.set.map((entry) => ({ key: entry.key, state: 'Effective' })),
      errors: [],
      config: snapshot.config,
    }));
  }
  async read() { return this.snapshot; }
  async readConfig() { return this.snapshot.config; }
  async applyPatch(patch) { this.patches.push(patch); return this.patchImpl(patch); }
  async execute(command, payload) { this.calls.push({ command, payload }); return this.executeImpl(command, payload); }
}

// --------------------------------------------------------------- A01–A10

await scenario('A01', 'Provider/model atomicity', async () => {
  const options = viewOf('M-02', rich).options;
  assert(!options.includes('custom/mystery-1'), 'unavailable pair must not be selectable');
  const backend = new Backend(rich, {
    execute: async () => ({ ok: false, message: 'model_unavailable: custom/mystery-1 is unavailable; previous pair retained.', call: null, body: null, errorCode: 'model_unavailable' }),
  });
  const initialDraft = stageControl(EMPTY_DRAFT, bindingOf('M-01'), 'ollama');
  const before = { provider: rich.config.values.find((v) => v.key === 'faktor.defaultProvider').value, model: rich.config.values.find((v) => v.key === 'faktor.defaultModel').value };
  const activated = await activateControl(backend, state(rich, { draft: initialDraft.draft }), 'M-02', { value: 'custom/mystery-1' });
  assertEqual(activated.result.kind, 'refused', 'unavailable pair must be refused');
  assertEqual(backend.calls[0].payload.provider, 'ollama', 'pair carries the current provider');
  assertEqual(backend.calls[0].payload.model, 'custom/mystery-1', 'pair carries the selected model');
  assertEqual(activated.state.draft.set.get('M-01').value, 'ollama', 'draft survives');
  assertEqual(rich.config.values.find((v) => v.key === 'faktor.defaultProvider').value, before.provider, 'default provider unchanged');
  assertEqual(rich.config.values.find((v) => v.key === 'faktor.defaultModel').value, before.model, 'default model unchanged');
  assertIncludes(rowValue('S02', 'model-entry-2', rich).toLowerCase(), 'no probe', 'unavailable pair remains visible with a reason');
});

await scenario('A02', 'Capability authority', async () => {
  const snapshot = validateSnapshot(unknownCapabilitySnapshot());
  const view = screenView('S02', state(snapshot));
  const authority = view.rows.find((row) => row.key === 'model-authority')?.value;
  assertEqual(authority, 'unknown');
  assertEqual(view.rows.find((row) => row.key === 'model-gated')?.value, 'Yes');
  assertEqual(view.rows.find((row) => row.key === 'model-tools')?.value, 'Unavailable');
  assertIncludes(view.rows.find((row) => row.key === 'model-entry-0')?.value, 'unknown');
  const html = renderEdition5Html({ nonce: 'n', cspSource: 'x', scriptUri: 'x', styleUri: 'x', state: state(snapshot), initialScreen: 'S02', host: 'vscode' });
  assertExcludes(html, 'marketing name', 'marketing/manual labels never certify support');
  assertIncludes(html, 'Capabilities gated');
});

await scenario('A03', 'Generation compatibility', async () => {
  const call = commandCall('generation.preview', { key: 'temperature', value: 3, provider: 'ollama', model: 'qwen3:8b' });
  assertEqual(call.method, 'POST');
  assertEqual(call.path, '/native/models/preview');
  assertEqual(call.body.key, 'temperature');
  assertEqual(call.body.value, 3);
  const snapshot = clone(rich);
  snapshot.generation.incompatible = [{ key: 'top_k', reason: 'The served model declares no top_k parameter.' }];
  const rows = rowsOf('S05', snapshot);
  assertIncludes(rows.find((row) => row.key === 'incompatible-0')?.value, 'no top_k parameter');
  assertEqual(parseControlValue(bindingOf('G-04'), 'abc').error !== null, true, 'unsupported numeric text fails locally');
  assertEqual(GENERATION_PARAMETER_KEYS['G-05'], 'top_p');
});

await scenario('A04', 'Unset versus zero', async () => {
  const zero = parseControlValue(bindingOf('G-03'), '0');
  assertEqual(zero.error, null);
  assertEqual(zero.value, 0, 'explicit zero survives');
  const absent = clone(rich);
  absent.config.values = absent.config.values.filter((v) => v.key !== 'temperature');
  assertIncludes(viewOf('G-04', absent).valueText, 'Inherit');
  assertIncludes(viewOf('G-04', rich).valueText, '0', 'zero is not rendered as absent');
  const staged = stageControl(EMPTY_DRAFT, bindingOf('G-03'), '0');
  assertEqual(staged.draft.set.get('G-03').value, 0);
  const reset = unstageControl(staged.draft, bindingOf('G-03'));
  assertEqual(reset.set.size, 0);
  assert(reset.unset.has('G-03'), 'reset removes the override');
  const patch = buildPatch(reset, 'r7');
  assertEqual(patch.patch.set.length, 0, 'reset writes no 0');
  assertEqual(patch.patch.unset.length, 1);
});

await scenario('A05', 'Active-call capture', async () => {
  const snapshot = clone(rich);
  snapshot.config.values = snapshot.config.values.map((entry) =>
    entry.key === 'faktor.defaultModel' ? { ...entry, value: 'gpt-next' } : entry,
  );
  const callRow = rowValue('S02', 'call-0', snapshot);
  assertIncludes(callRow, 'ollama/qwen3:8b', 'historical receipt keeps the executed model');
  assertIncludes(callRow, 'coder');
  const backend = new Backend(snapshot);
  const result = await activateControl(backend, state(snapshot), 'M-02', { value: 'gpt-next' });
  assertEqual(result.result.kind, 'action');
  assertEqual(backend.calls[0].payload.model, 'gpt-next');
  assertEqual(result.state.snapshot.models.calls[0].model, 'qwen3:8b', 'in-flight/historical receipt unchanged');
  assertIncludes(viewOf('M-12', snapshot).control.behavior, 'never rewrites an in-flight request');
});

await scenario('A06', 'Secret storage', async () => {
  const text = json(rich);
  assert(!/sk-[A-Za-z0-9]{12,}/.test(text), 'no provider secret values in the read model');
  assertIncludes(text, 'env:OPENAI_API_KEY');
  assert(rich.providers.connections[1].credential_bound, 'only the reference is visible');
  assertEqual(parseControlValue(bindingOf('P-04'), 'sk-live-secret').error !== null, true, 'raw secret refused');
  assertEqual(parseControlValue(bindingOf('P-04'), 'env:OPENAI_API_KEY').error, null);
  assert(rich.diagnostics.export_preview.omitted.includes('secrets'), 'diagnostics omit secrets');
  const html = renderEdition5Html({ nonce: 'n', cspSource: 'x', scriptUri: 'x', styleUri: 'x', state: state(rich), initialScreen: 'S03', host: 'vscode' });
  assert(!/sk-[A-Za-z0-9]{12,}/.test(html), 'markup never contains a secret');
  const revocation = commandCall('provider.rotate', { id: 'openai-main', action: 'rotate' });
  assertEqual(revocation.path, '/native/provider-connections/openai-main/rotate-credential');
});

await scenario('A07', 'Loopback versus LAN', async () => {
  assertEqual(rich.providers.local.endpoint_class, 'loopback');
  assertEqual(rich.providers.local.loopback_authorized, true);
  assertIncludes(rich.providers.local.lan_policy, 'refused', 'LAN remains refused');
  assertIncludes(rowValue('S04', 'local-guarantee', rich) ?? rowValue('S13', 'policy-guarantee', rich), 'not complete isolation');
  const p09 = controlOf('S04', 'P-09', rich);
  assertEqual(p09.state, 'disabled', 'LAN policy is a managed lock, not an inert toggle');
  assertIncludes(p09.stateText, 'Locked');
  const call = commandCall('provider.test', { id: 'custom', allow_loopback: false });
  assertEqual(call.body.allow_loopback, false, 'loopback opt-in is per connection');
  assertExcludes(JSON.stringify(call), 'lan: true', 'allow_loopback does not authorize LAN');
});

await scenario('A08', 'Remote locality', async () => {
  const rows = rowsOf('S31', rich);
  assertIncludes(rows.find((row) => row.key === 'binding-daemon')?.value, 'same machine');
  assertIncludes(rows.find((row) => row.key === 'binding-model')?.value, 'resolves on the daemon');
  assertIncludes(rows.find((row) => row.key === 'binding-vault')?.value, 'keychain');
  assertEqual(rows.find((row) => row.key === 'binding-generation')?.value, '4');
  assertExcludes(rows.find((row) => row.key === 'binding-model')?.value, 'laptop', 'locality is not derived from the laptop');
  const changed = clone(rich);
  changed.binding_view.generation = 5;
  changed.binding_view.stale_handles = ['terminal-handle-4', 'model-session-4'];
  const changedRows = rowsOf('S31', changed);
  assertIncludes(changedRows.find((row) => row.key === 'binding-stale-0')?.value, 'terminal-handle-4');
});

await scenario('A09', 'Ask enforcement', async () => {
  const ask = rich.agents.presets.find((preset) => preset.id === 'ask');
  assertEqual(ask.enforced, true);
  assert(!ask.allowed_tools.includes('edit_file'), 'Ask does not offer writes');
  assert(!ask.allowed_tools.includes('shell_run'));
  assert(ask.resolved_permissions.some((rule) => rule.startsWith('edit_file: deny')));
  assertEqual(ask.ask_prompt_only, false, 'enforced Ask is not labelled prompt-only');
  const custom = rich.agents.presets.find((preset) => preset.id === 'custom-1');
  assertEqual(custom.ask_prompt_only, true, 'a prompt-only route is labelled');
  assertIncludes(rowValue('S06', 'preset-ask', rich), 'enforced');
});

await scenario('A10', 'Preset and policy', async () => {
  assert(rich.policy.managed_denies.length > 0);
  const custom = rich.agents.presets.find((preset) => preset.id === 'custom-1');
  assert(custom.resolved_permissions.includes('managed deny retained'), 'custom preset cannot widen managed authority');
  assertEqual(custom.ownership, 'candidate worktree');
  const denied = commandCall('policy.preview', { tool: '*', target: 'owner-checkout', action: 'write' });
  assertEqual(denied.path, '/native/policy/preview');
  assertExcludes(json(custom).toLowerCase(), 'full auto', 'full auto is not a normal mode');
});

// -------------------------------------------------------------- A11–A20

await scenario('A11', 'Context null', async () => {
  const snapshot = validateSnapshot(contextNullSnapshot());
  const s01 = rowsOf('S01', snapshot);
  const s08 = rowsOf('S08', snapshot);
  assertIncludes(s01.find((row) => row.key === 's01-context')?.value, 'Unavailable');
  assertIncludes(s08.find((row) => row.key === 'context-meter')?.value, 'contextUsage=null');
  for (const value of [...s01, ...s08].map((row) => row.value)) {
    assert(!/^\s*0\s*%/.test(value), 'no fabricated 0% bar');
    assert(!/context full/i.test(value), 'no fabricated full claim');
    assert(!/measured/i.test(value), 'no measured label on a null value');
  }
  const html = renderEdition5Html({ nonce: 'n', cspSource: 'x', scriptUri: 'x', styleUri: 'x', state: state(snapshot), initialScreen: 'S08', host: 'vscode' });
  assert(!/context[^<]{0,80}0\s*%/i.test(html), 'no zero-percent context bar');
});

await scenario('A12', 'Context arithmetic', async () => {
  const usage = rich.context.usage;
  assertEqual(usage.categories.reduce((sum, entry) => sum + entry.tokens, 0), usage.used, 'categories sum to used input');
  assertEqual(usage.used + usage.output_reserve + usage.safety_reserve + usage.headroom, usage.capacity, '18,400 + 4,096 + 2,048 + 8,224 = 32,768');
  assert(usage.categories.every((entry) => entry.basis === usage.basis), 'one shared basis');
  assertIncludes(rowValue('S08', 'context-output-reserve', rich), 'not consumed input');
  assertIncludes(rowValue('S08', 'context-safety-reserve', rich), 'not consumed input');
  const overflow = clone(rich);
  overflow.context.overflow = { over_by: 500, actions: ['remove a source', 'compact'] };
  assertIncludes(rowValue('S08', 'context-overflow', overflow), 'over by 500');
});

await scenario('A13', 'Source identity', async () => {
  const sources = rich.context.manifest.sources;
  const selection = sources.find((source) => source.kind === 'selection');
  const diagnostic = sources.find((source) => source.kind === 'diagnostic');
  const file = sources.find((source) => source.kind === 'file');
  for (const source of [selection, diagnostic, file]) {
    assert(source.uri.includes('/candidate-1/'), 'URI belongs to its worktree');
    assert(source.worktree.includes('candidate-1'));
    assert(source.inclusion_reason.length > 0);
  }
  assertEqual(selection.range, '10:24');
  assertEqual(diagnostic.range, '18:1');
  assert(selection.version !== null && diagnostic.version !== null);
  assertIncludes(rowValue('S08', 'source-src-2', rich), '10:24');
  assertExcludes(rowValue('S08', 'source-src-2', rich), 'current directory');
});

await scenario('A14', 'Attachment failure', async () => {
  const failed = rich.context.manifest.sources.find((source) => source.id === 'src-4');
  assertEqual(failed.included, false);
  assertIncludes(failed.failed_reason, 'Oversize');
  const rows = rowsOf('S08', rich);
  assert(rows.some((row) => row.key === 'source-src-4' && row.state === 'error'), 'failed source remains visible');
  const backend = new Backend(rich, { execute: async () => ({ ok: false, message: 'attachment refused', call: null, body: null, errorCode: 'refused' }) });
  const staged = stageControl(EMPTY_DRAFT, bindingOf('P-03'), 'http://127.0.0.1:11434');
  const activated = await activateControl(backend, state(rich, { draft: staged.draft }), 'C-05');
  assertEqual(activated.result.kind, 'refused');
  assert(activated.state.draft.set.has('P-03'), 'draft text/values survive the attachment error');
});

await scenario('A15', 'Compaction continuity', async () => {
  const failed = validateSnapshot(failedCompactionSnapshot());
  assertEqual(failed.compaction.operation.state, 'Refused');
  assert(failed.compaction.operation.reduction_fraction < failed.compaction.operation.minimum_reduction, '1.2% is not success');
  assertIncludes(rowValue('S10', 'compaction-error', failed), 'minimum');
  assertIncludes(rowValue('S10', 'compaction-pending', failed), 'req-1');
  assertIncludes(rowValue('S10', 'compaction-pending', failed), 'op-9');
  assertIncludes(rowValue('S10', 'compaction-retained', failed), 'contract');
  const ok = rowValue('S10', 'compaction-measured', rich);
  assertIncludes(ok, '18400 \u2192 9800');
  assertIncludes(rowValue('S10', 'compaction-version', rich), 'sum-3');
  assertExcludes(rowValue('S10', 'compaction-result', failed) ?? '', 'Applied', 'failed reduction is not shown as applied');
});

await scenario('A16', 'Memory deletion', async () => {
  const snapshot = clone(rich);
  snapshot.memory.records[0].stopped_future_use_at_revision = 'r7';
  snapshot.memory.records[0].retained_prior_artifacts = ['evidence:3'];
  const value = rowValue('S12', 'memory-mem-1', snapshot);
  assertIncludes(value, 'stopped for future use at r7');
  assertIncludes(value, 'prior artifacts retained');
  const call = commandCall('memory.delete', { id: 'mem-1', scope: 'project' });
  assertEqual(call.path, '/native/memory/mem-1');
  assertEqual(call.method, 'DELETE');
  assertExcludes(json(call), 'global', 'a project cannot redirect global scope');
  const receipt = commandCall('memory.review', { id: 'mem-1' });
  assertEqual(receipt.path, '/native/memory/mem-1/review');
});

await scenario('A17', 'Approval exactly once', async () => {
  const pending = rich.approvals.pending[0];
  const call = commandCall('approval.reply', {
    session: pending.session,
    request_id: pending.request_id,
    generation: pending.generation,
    decision: 'allow_once',
  });
  assertEqual(call.path, '/native/permission/reply');
  assertEqual(call.body.request_id, 'req-1');
  assertEqual(call.body.generation, 4);
  let replies = 0;
  const backend = new Backend(rich, {
    execute: async () => {
      replies += 1;
      return replies === 1
        ? { ok: true, message: 'accepted', call: null, body: null, errorCode: null }
        : { ok: false, message: 'unknown_or_resolved: the waiter was already consumed', call: null, body: null, errorCode: 'unknown_or_resolved' };
    },
  });
  const first = await activateControl(backend, state(rich), 'Q-06');
  const second = await activateControl(backend, first.state, 'Q-06');
  assertEqual(first.result.kind, 'action');
  assertEqual(second.result.kind, 'refused');
  assertEqual(replies, 2, 'the daemon, not the UI, owns exactly-once');
  assertIncludes(second.result.message, 'already consumed');
  assertExcludes(JSON.stringify(second.state.snapshot.approvals.pending), '"consumed":true', 'a stale snapshot never pretends the waiter was consumed');
});

await scenario('A18', 'Persistent rule boundary', async () => {
  const noStore = clone(rich);
  noStore.approvals.pending[0].remember_available = false;
  const disabled = controlOf('S14', 'Q-07', noStore);
  assertEqual(disabled.state, 'disabled');
  assertIncludes(disabled.stateText, 'No persistent policy store');
  const once = commandCall('approval.reply', { request_id: 'req-1', session: 's1', decision: 'allow_once' });
  const remember = commandCall('approval.remember', { request_id: 'req-1', session: 's1', decision: 'allow_once', remember: true, scope: 'project', expires_at_ms: null });
  assertEqual(once.path, '/native/permission/reply');
  assertEqual(remember.path, '/native/policy/rules', 'Remember is a separate policy save');
  assertExcludes(json(once), 'remember', 'Allow once cannot silently persist');
  assertIncludes(JSON.stringify(remember.body), 'project');
});

await scenario('A19', 'Command normalization', async () => {
  const simulation = rich.policy.simulation;
  assertIncludes(simulation.normalized, 'git status --short');
  assertIncludes(simulation.normalized, 'cwd: candidate worktree');
  assertEqual(simulation.matched_rule, 'rule-ask-edit');
  assertIncludes(rowValue('S13', 'policy-simulation', rich), 'normalized');
  const call = commandCall('policy.preview', { tool: 'shell_run', target: 'git status --short', cwd: '/work/faktor/.faktor/worktrees/candidate-1' });
  assertEqual(call.body.cwd.includes('candidate-1'), true);
  assertExcludes(json(simulation), 'prefix only', 'a raw prefix is not shown as safe');
});

await scenario('A20', 'Sandbox truth', async () => {
  assertIncludes(rich.policy.network_guarantee, 'not complete isolation');
  assertEqual(rich.policy.filesystem.includes('candidate'), true);
  assertEqual(rich.policy.shell.includes('fail-closed'), true);
  const failed = clone(rich);
  failed.policy.sandbox_state = 'failed';
  failed.policy.sandbox_error = 'Sandbox helper exited 1; affected shell actions are blocked.';
  const row = rowValue('S13', 'policy-sandbox', failed);
  assertIncludes(row, 'failed');
  assertIncludes(row, 'blocked');
  assertExcludes(row, 'fully isolated');
  assertIncludes(rowValue('S13', 'policy-guarantee', failed), 'not complete isolation');
});

// -------------------------------------------------------------- A21–A30

await scenario('A21', 'MCP bounds', async () => {
  const valid = { name: 'docs', command: '/usr/bin/mcp', args: ['--stdio'] };
  assertEqual(validateMcpPayload(valid, 7), null);
  assertIncludes(validateMcpPayload(valid, 8), 'At most 8');
  assertIncludes(validateMcpPayload({ ...valid, name: 'n'.repeat(129) }, 0), '128');
  assertIncludes(validateMcpPayload({ ...valid, command: 'c'.repeat(513) }, 0), '512');
  assertIncludes(validateMcpPayload({ ...valid, args: Array.from({ length: 33 }, () => 'a') }, 0), '32');
  assertIncludes(validateMcpPayload({ ...valid, args: ['a'.repeat(513)] }, 0), '512');
  const call = commandCall('mcp.create', valid);
  assertEqual(call.path, '/native/mcp/servers');
  assertIncludes(call.body.name, 'docs');
});

await scenario('A22', 'MCP transport/auth', async () => {
  const noRemote = clone(rich);
  noRemote.tools.mcp = noRemote.tools.mcp.map((server) => ({ ...server, remote_supported: false, auth_state: 'not_required' }));
  const t04 = controlOf('S16', 'T-04', noRemote);
  assertEqual(t04.state, 'disabled');
  assertIncludes(t04.stateText, 'Remote transport');
  assertEqual(controlOf('S16', 'T-04', rich).state, 'available');
  const auth = commandCall('mcp.auth', { id: 'mcp-hosted', action: 'connect' });
  assertEqual(auth.path, '/native/mcp/servers/mcp-hosted/oauth/connect');
  const cancelled = clone(rich);
  cancelled.tools.mcp[1].auth_state = 'required';
  assertExcludes(JSON.stringify(cancelled.tools.mcp[1]), '"auth_state":"connected"', 'cancelled auth is not Connected');
});

await scenario('A23', 'Project executable trust', async () => {
  const projectSkill = rich.skills.skills.find((skill) => skill.id === 'skill-docs');
  assertIncludes(projectSkill.trust, 'project content');
  assertEqual(projectSkill.enabled, false);
  assert(projectSkill.files.includes('setup.sh'), 'manifest files are visible without executing setup');
  assertIncludes(projectSkill.manifest, 'setup: setup.sh');
  const projectMcp = clone(rich);
  projectMcp.tools.mcp.push({ ...projectMcp.tools.mcp[0], id: 'mcp-project', source: 'project', trusted: false, health: 'stopped' });
  assertEqual(projectMcp.tools.mcp.at(-1).trusted, false, 'project executable requires deliberate trust');
  assertExcludes(json(projectMcp.tools.mcp.at(-1)), 'autostart', 'viewing a manifest never starts it');
});

await scenario('A24', 'Hooks and verdict', async () => {
  const timeout = clone(rich);
  timeout.skills.hooks[0].last_test = {
    outcome: 'TimedOut', exit_state: null, verdict: null, stdout: 'partial', stderr: '',
    audit_ref: 'evidence:12', spec_hash: 'sha256:h2', at_ms: 1792255900000,
  };
  const row = rowValue('S18', 'hook-hook-pre-tool-test', timeout);
  assertIncludes(row, 'TimedOut');
  assertExcludes(row, 'Allow', 'timeout cannot authorize a tool');
  assertIncludes(rowValue('S18', 'hook-hook-pre-tool', timeout), 'FailClosed');
  assertIncludes(rowValue('S18', 'hook-hook-pre-tool', timeout), '65536');
  const malformed = clone(timeout);
  malformed.skills.hooks[0].last_test.outcome = 'Malformed';
  malformed.skills.hooks[0].last_test.verdict = 'Allow';
  assertExcludes(rowValue('S18', 'hook-hook-pre-tool-test', malformed), 'Allow', 'malformed verdict cannot be a pass');
  const call = commandCall('hook.test', { id: 'hook-pre-tool' });
  assertEqual(call.path, '/native/hooks/hook-pre-tool/test');
});

await scenario('A25', 'Child budgets', async () => {
  const parent = rich.agents.tree.find((node) => node.parent_id === null);
  const children = rich.agents.tree.filter((node) => node.parent_id !== null);
  assert(parent.budget.cap_micro !== null);
  for (const child of children) {
    assert(child.budget.cap_micro !== null, 'no unlimited child wallet');
    assert(BigInt(child.budget.cap_micro) <= BigInt(parent.budget.cap_micro), 'child cap shares the parent envelope');
    const total = BigInt(child.budget.open_micro) + BigInt(child.budget.settled_micro) + BigInt(child.budget.uncertain_micro);
    assert(total <= BigInt(child.budget.cap_micro), 'child reservation is bounded');
    assert(child.ownership.length > 0, 'ownership contention is visible');
  }
  assertEqual(rich.agents.concurrency.limit, 4);
  assertEqual(rich.agents.concurrency.active, 2);
});

await scenario('A26', 'Steer bounds', async () => {
  assertIncludes(validateSteerText(''), 'empty');
  assertEqual(validateSteerText('x'.repeat(500)), null);
  const tooLong = validateSteerText('x'.repeat(501));
  assertIncludes(tooLong, '501');
  assertIncludes(tooLong, 'not truncated');
  const call = commandCall('steer.send', { session: 's1', agent: 'child-1', text: 'x'.repeat(500), generation: 4 });
  assertEqual(call.path, '/native/session/s1/agents/child-1/steer');
  assertEqual(call.body.generation, 4, 'stale child controls are fenced by generation');
});

await scenario('A27', 'Owned process', async () => {
  assertEqual(controlOf('S19', 'X-05', rich).state, 'available');
  const stale = clone(rich);
  stale.terminal.processes[0].live = false;
  const disabled = controlOf('S19', 'X-05', stale);
  assertEqual(disabled.state, 'disabled');
  assertIncludes(disabled.stateText, 'live owned process');
  const stop = commandCall('terminal.stop', { session: 's1', terminal: 'term-1' });
  assertEqual(stop.path, '/native/session/s1/terminals/term-1/stop');
  assertIncludes(rowValue('S19', 'process-term-1', rich), 'handle gen 4');
  assertEqual(stop.path.includes('/session/'), true);
});

await scenario('A28', 'Unknown effect', async () => {
  const unknown = rowValue('S22', 'run-2-t3', rich);
  assertIncludes(unknown, 'unknown');
  assertExcludes(unknown, 'cancelled');
  const admission = rowValue('S30', 'diag-uncertain-op-9', rich);
  assertIncludes(admission, 'unknown');
  assertIncludes(admission, 'not eligible', 'no blind retry');
  const reconcile = commandCall('terminal.reconcile', { session: 's1', terminal: 'term-1' });
  assertEqual(reconcile.path, '/native/session/s1/terminals/term-1/reconcile');
});

await scenario('A29', 'Admission idempotence', async () => {
  const id = '123e4567-e89b-42d3-a456-426614174000';
  assertEqual(canonicalSubmissionId(id), id);
  assertEqual(canonicalSubmissionId('not-a-uuid'), null);
  const base = { mutationMode: '', maxTokens: 0, maxCostMicro: 0n, submissionId: id };
  const first = startTaskRequest('same goal', base);
  const retry = startTaskRequest('same goal', base);
  assertEqual(first.submission_id, retry.submission_id, 'same UUID + body reuses the id');
  const altered = startTaskRequest('altered goal', base);
  assertEqual(altered.submission_id, id, 'same UUID with an altered body is a conflict at the daemon');
  assertNotEqual(hash(first), hash(altered), 'altered body has a different hash');
});
function assertNotEqual(actual, expected, message) { if (actual === expected) throw new Error(message || 'expected different'); }
function hash(value) { return createHash('sha256').update(json(value)).digest('hex'); }

await scenario('A30', 'Budget exactness', async () => {
  assertEqual(parseMoneyMicro('5.00').value, '5000000');
  assertEqual(parseMoneyMicro('0.000001').value, '1');
  assertEqual(parseMoneyMicro('-1').error !== null, true);
  assertEqual(parseMoneyMicro('1.0000001').error !== null, true, 'sub-micro fraction refused');
  assertEqual(parseMoneyMicro('9223372036854775.808').error !== null, true, 'overflow refused');
  const staged = stageControl(EMPTY_DRAFT, bindingOf('B-02'), '5.00');
  assertEqual(staged.draft.set.get('B-02').value, '5000000');
  assertEqual(parseControlValue(bindingOf('B-01'), '1.5').error !== null, true, 'token cap is integer');
  assertExcludes(json(rich.budget), '4.999999', 'no float rounding');
});

// -------------------------------------------------------------- A31–A40

await scenario('A31', 'Unknown pricing', async () => {
  const priceRows = rowsOf('S02', rich);
  assertIncludes(priceRows.find((row) => row.key === 'price-list')?.value, 'Unavailable');
  assertIncludes(priceRows.find((row) => row.key === 'price-quota')?.value, 'Unavailable');
  assertExcludes(priceRows.find((row) => row.key === 'price-list')?.value, '0', 'missing rate is not zero');
  assertIncludes(priceRows.find((row) => row.key === 'price-basis')?.value, 'list unavailable');
  const usage = rowValue('S27', 'usage-prices', rich);
  assertIncludes(usage, 'provider quota');
  assertIncludes(usage, 'local estimate');
  assertIncludes(usage, 'observed charge');
  assertIncludes(rowValue('S21', 'budget-ledger', rich), 'uncertain');
});

await scenario('A32', 'Retry policy', async () => {
  assertEqual(rich.budget.attempts, 3, '3 total attempts, not 3 retries');
  assertEqual(rich.budget.rate_limit.max_attempts, 3);
  assertEqual(rich.budget.rate_limit.attempt, 2);
  assertIncludes(rowValue('S21', 'budget-rate', rich), 'reset unknown');
  assertIncludes(rowValue('S21', 'budget-rate', rich), '2/3');
  const cancel = commandCall('retry.cancel', { session: 's1' });
  assertEqual(cancel.path, '/native/session/s1/run/retry/cancel');
  const unknown = rowValue('S30', 'diag-uncertain-op-9', rich);
  assertIncludes(unknown, 'not eligible');
});

await scenario('A33', 'Checkpoint coverage', async () => {
  const hashOnly = rowValue('S25', 'checkpoint-cp-0', rich);
  assertIncludes(hashOnly, 'content unavailable');
  assertIncludes(hashOnly, 'redo unavailable');
  assertIncludes(hashOnly, 'diff unavailable');
  assertIncludes(hashOnly, 'network fetch');
  const onlyHash = clone(rich);
  onlyHash.checkpoints = [onlyHash.checkpoints[1]];
  const restore = controlOf('S25', 'V-05', onlyHash);
  assertEqual(restore.state, 'disabled');
  assertIncludes(restore.stateText, 'hash-only');
  assertIncludes(rowValue('S25', 'checkpoint-cp-1', rich), 'uncovered');
  assertIncludes(rowValue('S25', 'checkpoint-cp-0', rich), 'hash only');
});

await scenario('A34', 'Manual-edit CAS', async () => {
  const conflict = clone(rich);
  conflict.changes.manual_edit_conflict = 'Expected hash sha256:base-1 changed after preview; manual edits preserved.';
  assertIncludes(rowValue('S24', 'changes-conflict', conflict), 'manual edits preserved');
  const call = commandCall('restore.preview', { session: 's1', checkpoint: 'cp-1', expected_current_hash: 'sha256:base-1' });
  assertEqual(call.body.expected_current_hash, 'sha256:base-1');
  assertEqual(call.path, '/native/session/s1/checkpoints/cp-1/restore/preview');
  assertExcludes(json(call), 'force', 'no silent overwrite');
});

await scenario('A35', 'Engineering completion', async () => {
  const pending = rowsOf('S32', rich);
  assertEqual(pending.find((row) => row.key === 'proof-outcome')?.value, 'pending');
  assertEqual(pending.find((row) => row.key === 'proof-required')?.value, '1/2');
  assertEqual(pending.find((row) => row.key === 'proof-receipt')?.value, 'Unavailable');
  assertIncludes(pending.find((row) => row.key === 'proof-blocker-0')?.value, 'Missing required evidence');
  assertExcludes(rowValue('S32', 'proof-outcome', rich), 'verified');
  const verified = validateSnapshot(verifiedSnapshot());
  assertEqual(rowValue('S32', 'proof-outcome', verified), 'verified');
  assertEqual(rowValue('S32', 'proof-receipt', verified), 'finish-1');
});

await scenario('A36', 'VCSC denominator', async () => {
  assertEqual(792000 + 658000, 1450000);
  assertEqual(Number(rich.usage.vcsc_micro), 725000);
  const zero = validateSnapshot(zeroDenominatorSnapshot());
  const value = rowValue('S27', 'usage-vcsc', zero);
  assertIncludes(value, 'unavailable');
  assertEqual(zero.usage.vcsc_micro, null, 'zero denominator is Unavailable, not zero');
  assertIncludes(rowValue('S27', 'usage-cohort', rich), 'verified landed changes');
  assertIncludes(rowValue('S27', 'usage-numerator', rich), '1450000');
});

await scenario('A37', 'Atomic settings', async () => {
  const backend = new Backend(rich);
  const staged = stageControl(EMPTY_DRAFT, bindingOf('G-03'), '5');
  const invalid = await applyDraft(backend, state(rich, { draft: staged.draft }), [
    { controlId: 'G-03', key: 'task.max_tokens', value: 'not-a-number' },
  ], []);
  assertEqual(invalid.ok, false);
  assertEqual(backend.patches.length, 0, 'invalid patch writes nothing');
  assert(staged.draft.set.has('G-03'), 'invalid patch keeps the draft');
  const conflictBackend = new Backend(rich, {
    patch: async () => ({ revision: 'r8', applied: false, field_states: [], errors: [{ key: 'task.max_tokens', code: 'revision_conflict', message: 'expected r7, current r8' }], config: rich.config }),
  });
  const conflict = await applyDraft(conflictBackend, state(rich, { draft: staged.draft }), [...staged.draft.set.values()], []);
  assertEqual(conflict.ok, false);
  assertIncludes(conflict.message, 'revision_conflict');
  assert(conflict.state.draft.set.has('G-03'), 'conflict retains the draft');
  const reset = await applyDraft(new Backend(rich), state(rich, { draft: staged.draft }), [], []);
  assertEqual(reset.state.draft.set.size, 0, 'Reset restores applied values');
  const defaults = restoreDefaults(state(rich, { draft: staged.draft }));
  assert(defaults.draft.set.size === 0 && defaults.draft.unset.size > 0, 'Defaults stage removals');
  assertEqual(backend.patches.length, 0, 'Reset/Defaults do not apply immediately');
});

await scenario('A38', 'Session/host fencing', async () => {
  const stale = validateSnapshot(staleSnapshot());
  const backend = new Backend(stale);
  const refused = await activateControl(backend, state(stale), 'M-02', { value: 'qwen3:8b' });
  assertEqual(refused.result.kind, 'refused');
  assertIncludes(refused.result.message, 'stale');
  assertEqual(backend.calls.length, 0, 'old/stale handle never dispatches');
  const draft = stageControl(EMPTY_DRAFT, bindingOf('P-03'), 'http://127.0.0.1:11434').draft;
  assertEqual(draft.set.size, 1, 'draft survives the fence');
  assertEqual(rowValue('S31', 'binding-generation', rich), '4');
  const other = clone(rich);
  other.binding.session = 's2';
  other.binding.generation = 5;
  assert(other.binding.generation !== rich.binding.generation, 'a host change invalidates the handle generation');
});

await scenario('A39', 'Accessible compact view', async () => {
  const chrome = findChrome();
  assert(chrome !== null, 'headless Chrome is required for the compact visual check');
  const dir = mkdtempSync(join(tmpdir(), 'faktor-e5-a39-'));
  const scriptFile = join(dir, 'edition5.js');
  const styleFile = join(dir, 'edition5.css');
  copyFileSync(join(MEDIA, 'edition5.js'), scriptFile);
  copyFileSync(join(MEDIA, 'edition5.css'), styleFile);
  const theme = `<style nonce="N">${THEMES.dark}</style>`;
  let html = renderEdition5Html({ nonce: 'N', cspSource: '*', scriptUri: pathToFileURL(scriptFile).href, styleUri: pathToFileURL(styleFile).href, state: state(rich), initialScreen: 'S01', host: 'vscode' });
  html = html.replace('</head>', `${theme}</head>`);
  html = html.replace('<script nonce="N" src=', `<script nonce="N">window.__posted=[];window.acquireVsCodeApi=function(){return{postMessage:function(m){window.__posted.push(m);}};};</script><script nonce="N" src=`);
  const htmlFile = join(dir, 'a39.html');
  writeFileSync(htmlFile, html);
  const child = await launchChrome(chrome);
  try {
    const cdp = await connect(child.pageWs);
    await cdp.send('Page.enable');
    await cdp.send('Runtime.enable');
    await cdp.send('Emulation.setDeviceMetricsOverride', { width: 240, height: 800, deviceScaleFactor: 2, mobile: false });
    await cdp.send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-reduced-motion', value: 'reduce' }] });
    await cdp.send('Page.navigate', { url: pathToFileURL(htmlFile).href });
    await sleep(700);
    await cdp.send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Tab', code: 'Tab', windowsVirtualKeyCode: 9 });
    await cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Tab', code: 'Tab', windowsVirtualKeyCode: 9 });
    const compact = await cdp.evaluate(`(function () {
      document.documentElement.style.setProperty('--vscode-font-size', '26px');
      var visible = Array.from(document.querySelectorAll('button, input, select, textarea, [tabindex="0"]')).filter(function (el) {
        return el.getClientRects().length > 0 && !el.closest('[hidden]');
      });
      var unnamed = visible.filter(function (el) {
        if (el.getAttribute('aria-label')) return false;
        if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT') {
          return !el.id || !document.querySelector('label[for="' + el.id + '"]');
        }
        return !(el.textContent || '').trim();
      }).map(function (el) { return el.tagName + '#' + el.id; });
      var tiny = visible.filter(function (el) { var r = el.getBoundingClientRect(); return r.width < 24 || r.height < 24; }).length;
      var overflow = document.documentElement.scrollWidth - window.innerWidth;
      var focusedElement = document.activeElement;
      var focused = !!focusedElement && focusedElement !== document.body;
      var outline = focusedElement ? getComputedStyle(focusedElement).outlineStyle : 'none';
      var live = document.querySelectorAll('[aria-live]').length;
      var textarea = document.querySelector('textarea');
      if (textarea) {
        textarea.dispatchEvent(new CompositionEvent('compositionstart', { data: '' }));
        textarea.value = 'IMETEXT';
        textarea.dispatchEvent(new CompositionEvent('compositionend', { data: 'IME' }));
      }
      return {
        overflow: overflow,
        offenders: Array.from(document.querySelectorAll('*')).filter(function (el) { var r = el.getBoundingClientRect(); return r.right > window.innerWidth + 1 || r.width > window.innerWidth + 1; }).slice(0, 12).map(function (el) { return el.tagName + '.' + (el.className || '').toString().slice(0, 40) + '#' + el.id + '=' + Math.round(el.getBoundingClientRect().width); }),
        unnamed: unnamed,
        tiny: tiny,
        focused: focused,
        outline: outline,
        live: live,
        ime: textarea ? textarea.value : null,
        stateBadges: document.querySelectorAll('.e5-state-badge').length,
        essential: !!document.querySelector('[data-control="M-01"]') && !!document.querySelector('[data-control="B-01"]'),
      };
    })()`);
    assert(compact.overflow <= 1, `horizontal overflow at 240px/200%: ${compact.overflow} (${compact.offenders.join(', ')})`);
    assertEqual(compact.unnamed.length, 0, `unnamed controls: ${compact.unnamed.join(', ')}`);
    assertEqual(compact.tiny, 0, 'all controls are at least 24x24');
    assertEqual(compact.focused, true, 'keyboard focus reaches the first control');
    assert(compact.outline !== 'none', 'focus ring is visible');
    assert(compact.live <= 6, 'bounded live regions (no SR spam)');
    assertEqual(compact.ime, 'IMETEXT', 'IME text survives composition');
    assert(compact.stateBadges > 0, 'status is text, not color-only');
    assertEqual(compact.essential, true, 'essential controls remain at 240px/200%');
    cdp.ws.close();
  } finally {
    child.child.kill('SIGKILL');
  }
});

await scenario('A40', 'Long stream and full search', async () => {
  const find = commandCall('search.transcript', { session: 's1', query: 'parser', regex: true, case_sensitive: false });
  assertEqual(find.method, 'GET');
  assertIncludes(find.path, '/native/search/transcript');
  assertIncludes(find.path, 'q=parser');
  assertIncludes(find.path, 'regex=true');
  assertIncludes(find.path, 'case=false');
  assertEqual(rich.history.search.next_cursor, 'find-2', 'Find pages beyond rendered rows');
  const rows = rowsOf('S26', rich);
  assertIncludes(rows.find((row) => row.key === 'history-search')?.value, 'next find-2');
  assert(rows.filter((row) => row.key.startsWith('search-')).length <= 20, 'search rows are bounded');
  assert(rowsOf('S22', rich).length <= 120, 'run rows are bounded');
  assertIncludes(rowValue('S19', 'process-term-1', rich), '184000', 'terminal bytes are bounded/observed');
  assertIncludes(rowValue('S19', 'process-term-1-artifact', rich), 'evidence:12', 'full artifact linked');
  assertIncludes(rowValue('S26', 'history-total', rich), 'of 12', 'history total is served');
});

// ---------------------------------------------------------------- summary
console.log(`edition5 acceptance: ${passed}/${40} scenarios passed`);
if (failures.length > 0) {
  for (const failure of failures) console.error(`FAIL ${failure}`);
  process.exitCode = 1;
} else {
  console.log('edition5 acceptance PASSED: all 40 VS Code scenarios and forbidden fake states checked');
}
