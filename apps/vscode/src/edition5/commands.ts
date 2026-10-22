// Exact daemon endpoint for every Edition 5 command, plus the deterministic
// default payload the panel sends when the operator has not staged a more
// specific target. Missing selections are a typed error, never a guessed id.

import type { E5CommandKind } from './bindings.ts';
import type { E5ApprovalRequest, E5Snapshot, Json } from './contract.ts';

export interface E5CommandCall {
  readonly method: 'GET' | 'POST' | 'PATCH' | 'PUT' | 'DELETE';
  readonly path: string;
  readonly body: Readonly<Record<string, Json>> | null;
}

export type E5Payload = Readonly<Record<string, Json>>;

const READ_PATH = '/native/ui/edition5';

/** Exact MCP persistence bounds (A21); the error names the violated bound. */
export function validateMcpPayload(payload: E5Payload, existingServers: number): string | null {
  const name = typeof payload.name === 'string' ? payload.name : '';
  const command = typeof payload.command === 'string' ? payload.command : '';
  const args = Array.isArray(payload.args) ? payload.args.filter((entry): entry is string => typeof entry === 'string') : [];
  const bytes = (value: string) => new TextEncoder().encode(value).length;
  if (payload.id === undefined && existingServers >= 8) return 'At most 8 MCP servers are persisted; the 9th was refused before any write.';
  if (bytes(name) === 0) return 'MCP name is required.';
  if (bytes(name) > 128) return `MCP name is ${bytes(name)} UTF-8 bytes; the bound is 128.`;
  if (bytes(command) === 0) return 'MCP command is required.';
  if (bytes(command) > 512) return `MCP command is ${bytes(command)} UTF-8 bytes; the bound is 512.`;
  if (args.length > 32) return `MCP args has ${args.length} entries; the bound is 32.`;
  for (const [index, arg] of args.entries()) {
    if (bytes(arg) > 512) return `MCP arg ${index + 1} is ${bytes(arg)} UTF-8 bytes; the bound is 512.`;
  }
  return null;
}

/**
 * The pending request a decision button was rendered from. The approval
 * toolbar carries the EXACT request id of that render; a reply is refused
 * when that id is no longer live rather than silently selecting a newer
 * "first" request the human never saw.
 */
export function pendingApprovalById(
  snapshot: E5Snapshot | null,
  requestId: unknown,
): E5ApprovalRequest | null {
  if (typeof requestId !== 'string' || requestId.length === 0) return null;
  return (snapshot?.approvals?.pending ?? []).find((request) => request.request_id === requestId) ?? null;
}

/** Steer accepts 1..500 characters; it is rejected, never truncated (A26). */
export function validateSteerText(value: unknown): string | null {
  if (typeof value !== 'string') return 'Enter a steer message.';
  if (value.length === 0) return 'Steer is empty; nothing was sent.';
  if (value.length > 500) return `Steer is ${value.length} characters; the bound is 500 and the text was not truncated.`;
  return null;
}

function segment(value: string): string {
  return encodeURIComponent(value);
}

function requireString(payload: E5Payload, key: string): string {
  const value = payload[key];
  if (typeof value !== 'string' || value.length === 0) {
    throw new Error(`missing required selection "${key}" for this control`);
  }
  return value;
}

function optionalString(payload: E5Payload, key: string): string | null {
  const value = payload[key];
  return typeof value === 'string' && value.length > 0 ? value : null;
}

function body(payload: E5Payload): E5Payload {
  const copy: Record<string, Json> = {};
  for (const [key, value] of Object.entries(payload)) {
    if (key !== 'session' && key !== 'control_id' && key !== 'screen_id') copy[key] = value;
  }
  return copy;
}

function sessionPath(payload: E5Payload, suffix: string): string {
  return `/native/session/${segment(requireString(payload, 'session'))}${suffix}`;
}

/**
 * Build the exact native call for one command. `session` is optional for
 * daemon-scoped commands and required for session-scoped routes.
 */
export function commandCall(command: E5CommandKind, payload: E5Payload): E5CommandCall {
  switch (command) {
    case 'inspect':
    case 'approval.refresh':
    case 'session.switch':
      return {
        method: 'GET',
        path: `${READ_PATH}?session=${segment(requireString(payload, 'session'))}`,
        body: null,
      };
    case 'provider.create':
      return { method: 'POST', path: '/native/provider-connections', body: body(payload) };
    case 'provider.update':
      return {
        method: 'PATCH',
        path: `/native/provider-connections/${segment(requireString(payload, 'id'))}`,
        body: body(payload),
      };
    case 'provider.delete':
      return {
        method: 'DELETE',
        path: `/native/provider-connections/${segment(requireString(payload, 'id'))}`,
        body: null,
      };
    case 'provider.test':
      return {
        method: 'POST',
        path: `/native/provider-connections/${segment(requireString(payload, 'id'))}/test`,
        body: body(payload),
      };
    case 'provider.rotate':
      return {
        method: 'POST',
        path: `/native/provider-connections/${segment(requireString(payload, 'id'))}/rotate-credential`,
        body: body(payload),
      };
    case 'provider.discover':
      return {
        method: 'POST',
        path: `/native/provider-connections/${segment(requireString(payload, 'id'))}/discover-models`,
        body: body(payload),
      };
    case 'catalog.refresh':
      return { method: 'POST', path: '/native/catalog/refresh', body: body(payload) };
    case 'catalog.probe':
      return { method: 'POST', path: '/native/catalog/probe', body: body(payload) };
    case 'model.select':
      return { method: 'POST', path: '/native/model-selection', body: body(payload) };
    case 'model.favorite':
      return { method: 'POST', path: '/native/models/favorites', body: body(payload) };
    case 'generation.preview':
      return { method: 'POST', path: '/native/models/preview', body: body(payload) };
    case 'generation.reset':
      return { method: 'DELETE', path: '/native/model-tuning', body: null };
    case 'preset.select':
      return { method: 'POST', path: sessionPath(payload, '/preset'), body: body(payload) };
    case 'preset.save':
      return {
        method: 'PUT',
        path: `/native/agent-presets/${segment(requireString(payload, 'preset'))}`,
        body: body(payload),
      };
    case 'preset.duplicate':
      return {
        method: 'POST',
        path: `/native/agent-presets/${segment(requireString(payload, 'preset'))}/duplicate`,
        body: body(payload),
      };
    case 'role.save':
      return {
        method: 'PUT',
        path: `/native/agent-roles/${segment(requireString(payload, 'role'))}`,
        body: body(payload),
      };
    case 'context.setEditorContext':
      return { method: 'POST', path: '/native/context/editor-context', body: body(payload) };
    case 'context.add':
      return { method: 'POST', path: '/native/context/sources', body: body(payload) };
    case 'context.remove':
      return {
        method: 'DELETE',
        path: `/native/context/sources/${segment(requireString(payload, 'source'))}`,
        body: null,
      };
    case 'context.retry':
      return {
        method: 'POST',
        path: `/native/context/sources/${segment(requireString(payload, 'source'))}/retry`,
        body: body(payload),
      };
    case 'context.pin':
      return {
        method: 'POST',
        path: `/native/context/sources/${segment(requireString(payload, 'source'))}/pin`,
        body: body(payload),
      };
    case 'context.exclude':
      return {
        method: 'POST',
        path: `/native/context/sources/${segment(requireString(payload, 'source'))}/exclude`,
        body: body(payload),
      };
    case 'context.export':
      return { method: 'POST', path: '/native/context/export', body: body(payload) };
    case 'context.preview':
      return { method: 'POST', path: '/native/context/preview', body: body(payload) };
    case 'context.reindex':
      return { method: 'POST', path: '/native/index/reindex', body: body(payload) };
    case 'compaction.preview':
      return { method: 'POST', path: sessionPath(payload, '/compaction/preview'), body: body(payload) };
    case 'compaction.run':
      return { method: 'POST', path: sessionPath(payload, '/compaction'), body: body(payload) };
    case 'compaction.cancel':
      return {
        method: 'POST',
        path: sessionPath(payload, `/compaction/${segment(requireString(payload, 'operation'))}/cancel`),
        body: body(payload),
      };
    case 'instruction.save':
      return {
        method: 'PUT',
        path: `/native/instructions/${segment(requireString(payload, 'source'))}`,
        body: body(payload),
      };
    case 'instruction.toggle':
      return {
        method: 'POST',
        path: `/native/instructions/${segment(requireString(payload, 'source'))}/toggle`,
        body: body(payload),
      };
    case 'instruction.preview':
      return { method: 'POST', path: '/native/instructions/preview', body: body(payload) };
    case 'instruction.import':
      return { method: 'POST', path: '/native/instructions/import', body: body(payload) };
    case 'instruction.test':
      return { method: 'POST', path: '/native/instructions/test', body: body(payload) };
    case 'memory.save':
      return { method: 'POST', path: '/native/memory', body: body(payload) };
    case 'memory.delete':
      return { method: 'DELETE', path: `/native/memory/${segment(requireString(payload, 'id'))}`, body: null };
    case 'memory.review':
      return {
        method: 'POST',
        path: `/native/memory/${segment(requireString(payload, 'id'))}/review`,
        body: body(payload),
      };
    case 'policy.preview':
      return { method: 'POST', path: '/native/policy/preview', body: body(payload) };
    case 'policy.apply':
      return { method: 'POST', path: '/native/policy/rules', body: body(payload) };
    case 'policy.revoke':
      return {
        method: 'POST',
        path: `/native/policy/rules/${segment(requireString(payload, 'rule'))}/revoke`,
        body: body(payload),
      };
    case 'policy.audit':
      return { method: 'GET', path: '/native/policy/audit', body: null };
    case 'approval.reply': {
      // The decision must name the exact request id and owning session the
      // panel bound at render time; a missing selection is a typed error.
      requireString(payload, 'request_id');
      requireString(payload, 'session');
      return { method: 'POST', path: '/native/permission/reply', body: body(payload) };
    }
    case 'approval.remember':
      return { method: 'POST', path: '/native/policy/rules', body: body(payload) };
    case 'mcp.create':
      return { method: 'POST', path: '/native/mcp/servers', body: body(payload) };
    case 'mcp.start':
      return { method: 'POST', path: `/native/mcp/servers/${segment(requireString(payload, 'id'))}/start`, body: body(payload) };
    case 'mcp.stop':
      return { method: 'POST', path: `/native/mcp/servers/${segment(requireString(payload, 'id'))}/stop`, body: body(payload) };
    case 'mcp.restart':
      return { method: 'POST', path: `/native/mcp/servers/${segment(requireString(payload, 'id'))}/restart`, body: body(payload) };
    case 'mcp.test':
      return { method: 'POST', path: `/native/mcp/servers/${segment(requireString(payload, 'id'))}/test`, body: body(payload) };
    case 'mcp.remove':
      return { method: 'DELETE', path: `/native/mcp/servers/${segment(requireString(payload, 'id'))}`, body: null };
    case 'mcp.auth':
      return {
        method: 'POST',
        path: `/native/mcp/servers/${segment(requireString(payload, 'id'))}/oauth/${segment(optionalString(payload, 'action') ?? 'connect')}`,
        body: body(payload),
      };
    case 'tool.enable':
      return { method: 'POST', path: `/native/tools/${segment(requireString(payload, 'tool'))}/enable`, body: body(payload) };
    case 'tool.permissions':
      return { method: 'PUT', path: `/native/tools/${segment(requireString(payload, 'tool'))}/permission`, body: body(payload) };
    case 'tool.logs':
      return { method: 'GET', path: `/native/mcp/servers/${segment(requireString(payload, 'id'))}/logs`, body: null };
    case 'tool.export':
      return { method: 'POST', path: '/native/tools/export', body: body(payload) };
    case 'skill.install':
      return { method: 'POST', path: '/native/skills/install', body: body(payload) };
    case 'skill.update':
      return { method: 'POST', path: `/native/skills/${segment(requireString(payload, 'id'))}/update`, body: body(payload) };
    case 'skill.remove':
      return { method: 'DELETE', path: `/native/skills/${segment(requireString(payload, 'id'))}`, body: null };
    case 'skill.enable':
      return { method: 'POST', path: `/native/skills/${segment(requireString(payload, 'id'))}/enable`, body: body(payload) };
    case 'skill.invoke':
      return { method: 'POST', path: `/native/skills/${segment(requireString(payload, 'id'))}/invoke`, body: body(payload) };
    case 'command.insert':
      return {
        method: 'POST',
        path: `/native/commands/${segment(requireString(payload, 'command'))}/insert`,
        body: body(payload),
      };
    case 'hook.save':
      return { method: 'PUT', path: `/native/hooks/${segment(requireString(payload, 'id'))}`, body: body(payload) };
    case 'hook.test':
      return { method: 'POST', path: `/native/hooks/${segment(requireString(payload, 'id'))}/test`, body: body(payload) };
    case 'terminal.background':
      return { method: 'POST', path: sessionPath(payload, `/terminals/${segment(requireString(payload, 'terminal'))}/background`), body: body(payload) };
    case 'terminal.stop':
      return { method: 'POST', path: sessionPath(payload, `/terminals/${segment(requireString(payload, 'terminal'))}/stop`), body: body(payload) };
    case 'terminal.input':
      return { method: 'POST', path: sessionPath(payload, `/terminals/${segment(requireString(payload, 'terminal'))}/input`), body: body(payload) };
    case 'terminal.reconcile':
      return { method: 'POST', path: sessionPath(payload, `/terminals/${segment(requireString(payload, 'terminal'))}/reconcile`), body: body(payload) };
    case 'browser.connect':
      return { method: 'POST', path: '/native/browser/connect', body: body(payload) };
    case 'browser.capture':
      return { method: 'POST', path: `/native/browser/sessions/${segment(requireString(payload, 'id'))}/capture`, body: body(payload) };
    case 'browser.diagnostics':
      return { method: 'GET', path: `/native/browser/sessions/${segment(requireString(payload, 'id'))}/observations`, body: null };
    case 'budget.revise':
      return { method: 'POST', path: sessionPath(payload, '/budget/revision'), body: body(payload) };
    case 'retry.cancel':
      return { method: 'POST', path: sessionPath(payload, '/run/retry/cancel'), body: body(payload) };
    case 'queue.edit':
      return { method: 'PATCH', path: sessionPath(payload, `/queue/${segment(requireString(payload, 'item'))}`), body: body(payload) };
    case 'queue.remove':
      return { method: 'DELETE', path: sessionPath(payload, `/queue/${segment(requireString(payload, 'item'))}`), body: null };
    case 'queue.next':
      return { method: 'POST', path: sessionPath(payload, `/queue/${segment(requireString(payload, 'item'))}/next`), body: body(payload) };
    case 'steer.send':
      return { method: 'POST', path: sessionPath(payload, `/agents/${segment(requireString(payload, 'agent'))}/steer`), body: body(payload) };
    case 'agent.pause':
      return { method: 'POST', path: sessionPath(payload, `/agents/${segment(requireString(payload, 'agent'))}/pause`), body: body(payload) };
    case 'agent.stop':
      return { method: 'POST', path: sessionPath(payload, `/agents/${segment(requireString(payload, 'agent'))}/stop`), body: body(payload) };
    case 'agent.model':
      return { method: 'POST', path: sessionPath(payload, `/agents/${segment(requireString(payload, 'agent'))}/model`), body: body(payload) };
    case 'change.review':
      return {
        method: 'POST',
        path: sessionPath(payload, `/changes/${segment(requireString(payload, 'path'))}/review`),
        body: body(payload),
      };
    case 'change.comment':
      return {
        method: 'POST',
        path: sessionPath(payload, `/changes/${segment(requireString(payload, 'path'))}/comments`),
        body: body(payload),
      };
    case 'change.hunkAccept':
      return {
        method: 'POST',
        path: sessionPath(payload, `/changes/${segment(requireString(payload, 'path'))}/hunks/accept`),
        body: body(payload),
      };
    case 'change.followup':
      return { method: 'POST', path: sessionPath(payload, '/changes/follow-up'), body: body(payload) };
    case 'restore.preview':
      return {
        method: 'POST',
        path: sessionPath(payload, `/checkpoints/${segment(requireString(payload, 'checkpoint'))}/restore/preview`),
        body: body(payload),
      };
    case 'restore.run':
      return {
        method: 'POST',
        path: sessionPath(payload, `/checkpoints/${segment(requireString(payload, 'checkpoint'))}/restore`),
        body: body(payload),
      };
    case 'fork.run':
      return {
        method: 'POST',
        path: sessionPath(payload, `/checkpoints/${segment(requireString(payload, 'checkpoint'))}/fork`),
        body: body(payload),
      };
    case 'redo.run':
      return {
        method: 'POST',
        path: sessionPath(payload, `/checkpoints/${segment(requireString(payload, 'checkpoint'))}/redo`),
        body: body(payload),
      };
    case 'session.rename':
      return { method: 'PATCH', path: sessionPath(payload, ''), body: body(payload) };
    case 'session.pin':
      return { method: 'POST', path: sessionPath(payload, '/pin'), body: body(payload) };
    case 'session.archive':
      return { method: 'POST', path: sessionPath(payload, '/archive'), body: body(payload) };
    case 'session.delete':
      return { method: 'DELETE', path: sessionPath(payload, ''), body: null };
    case 'session.export':
      return { method: 'POST', path: sessionPath(payload, '/export'), body: body(payload) };
    case 'session.fork':
      return { method: 'POST', path: sessionPath(payload, '/fork'), body: body(payload) };
    case 'search.titles':
      return {
        method: 'GET',
        path: `/native/sessions?query=${segment(optionalString(payload, 'query') ?? '')}`,
        body: null,
      };
    case 'search.transcript':
      return {
        method: 'GET',
        path: `${READ_PATH.replace('/native/ui/edition5', '/native/search/transcript')}?session=${segment(requireString(payload, 'session'))}&q=${segment(optionalString(payload, 'query') ?? '')}&regex=${payload.regex === true ? 'true' : 'false'}&case=${payload.case_sensitive === true ? 'true' : 'false'}`,
        body: null,
      };
    case 'usage.filter':
      return {
        method: 'GET',
        path: `/native/usage?cohort=${segment(optionalString(payload, 'cohort') ?? '')}&period=${segment(optionalString(payload, 'period') ?? '')}&worktree=${segment(optionalString(payload, 'worktree') ?? '')}`,
        body: null,
      };
    case 'routing.preview':
      return { method: 'POST', path: '/native/routing/preview', body: body(payload) };
    case 'routing.save':
      return { method: 'POST', path: '/native/routing', body: body(payload) };
    case 'settings.search':
      return { method: 'GET', path: `/native/settings/search?q=${segment(optionalString(payload, 'query') ?? '')}`, body: null };
    case 'settings.import':
      return { method: 'POST', path: '/native/config/import', body: body(payload) };
    case 'settings.export':
      return { method: 'GET', path: '/native/config/export', body: null };
    case 'diagnostics.preview':
      return { method: 'POST', path: '/native/diagnostics/export/preview', body: body(payload) };
    case 'diagnostics.export':
      return { method: 'POST', path: '/native/diagnostics/export', body: body(payload) };
    case 'diagnostics.lifecycle':
      return {
        method: 'POST',
        path: `/native/runtime/${segment(optionalString(payload, 'action') ?? 'start')}`,
        body: body(payload),
      };
    case 'binding.revalidate':
      return { method: 'POST', path: '/native/binding/revalidate', body: body(payload) };
    case 'binding.rebind':
      return { method: 'POST', path: '/native/binding/rebind', body: body(payload) };
    case 'finish.request':
      return { method: 'POST', path: sessionPath(payload, '/finish'), body: body(payload) };
    case 'config.set':
    case 'settings.apply':
      return { method: 'POST', path: '/native/config/patch', body: body(payload) };
    case 'host.action':
      throw new Error('host.action is a native VS Code action and has no daemon route');
    default: {
      const never: never = command;
      throw new Error(`edition5: unhandled command ${String(never)}`);
    }
  }
}

/** Deterministic target selection from the served snapshot. */
export function defaultPayload(command: E5CommandKind, snapshot: E5Snapshot | null): E5Payload {
  const payload: Record<string, Json> = {};
  if (snapshot?.binding?.session != null) payload.session = snapshot.binding.session;
  const first = <T>(list: readonly T[] | null | undefined): T | null => (list && list.length > 0 ? list[0] : null);
  switch (command) {
    case 'provider.update':
    case 'provider.delete':
    case 'provider.test':
    case 'provider.rotate':
    case 'provider.discover': {
      const connection = first(snapshot?.providers?.connections ?? []);
      if (connection !== null) payload.id = connection.id;
      break;
    }
    case 'preset.save':
    case 'preset.duplicate': {
      const preset = first(snapshot?.agents?.presets ?? []);
      payload.preset = preset?.id ?? 'custom';
      break;
    }
    case 'role.save': {
      const role = first(snapshot?.agents?.roles ?? []);
      payload.role = role?.role ?? 'coder';
      break;
    }
    case 'context.remove':
    case 'context.retry':
    case 'context.pin':
    case 'context.exclude': {
      const source = first(snapshot?.context?.manifest?.sources ?? []);
      if (source !== null) payload.source = source.id;
      break;
    }
    case 'compaction.cancel': {
      const operation = snapshot?.compaction?.operation ?? null;
      if (operation !== null) payload.operation = operation.id;
      break;
    }
    case 'instruction.save':
    case 'instruction.toggle': {
      const source = first(snapshot?.instructions?.sources ?? []);
      payload.source = source?.id ?? 'user';
      break;
    }
    case 'memory.delete':
    case 'memory.review': {
      const record = first(snapshot?.memory?.records ?? []);
      if (record !== null) payload.id = record.id;
      break;
    }
    case 'policy.revoke': {
      const rule = first(snapshot?.policy?.rules ?? []);
      if (rule !== null) payload.rule = rule.id;
      break;
    }
    case 'approval.reply':
    case 'approval.remember': {
      // The command builder can name the served pending row for fixtures and
      // direct SDK calls; the PANEL refuses a missing id from the webview and
      // binds every real decision to the id rendered on its own button.
      const request = first(snapshot?.approvals?.pending ?? []);
      if (request !== null) {
        payload.request_id = request.request_id;
        payload.session = request.session;
        payload.generation = request.generation;
        payload.action = command === 'approval.reply' ? 'allow_once' : 'remember';
        payload.decision = 'allow_once';
      }
      break;
    }
    case 'mcp.start':
    case 'mcp.stop':
    case 'mcp.restart':
    case 'mcp.test':
    case 'mcp.remove':
    case 'mcp.auth':
    case 'tool.logs': {
      const server = first(snapshot?.tools?.mcp ?? []);
      if (server !== null) payload.id = server.id;
      break;
    }
    case 'tool.enable':
    case 'tool.permissions': {
      const tool = first(snapshot?.tools?.builtin ?? []);
      if (tool !== null) payload.tool = tool.id;
      break;
    }
    case 'skill.update':
    case 'skill.remove':
    case 'skill.enable':
    case 'skill.invoke': {
      const skill = first(snapshot?.skills?.skills ?? []);
      if (skill !== null) payload.id = skill.id;
      break;
    }
    case 'command.insert': {
      const commandEntry = first(snapshot?.skills?.commands ?? []);
      if (commandEntry !== null) payload.command = commandEntry.name;
      break;
    }
    case 'hook.save':
    case 'hook.test': {
      const hook = first(snapshot?.skills?.hooks ?? []);
      payload.id = hook?.id ?? 'new-hook';
      if (hook !== null) payload.event = hook.event;
      break;
    }
    case 'terminal.background':
    case 'terminal.stop':
    case 'terminal.input':
    case 'terminal.reconcile': {
      const process = first(snapshot?.terminal?.processes ?? []);
      if (process !== null) payload.terminal = process.id;
      break;
    }
    case 'browser.capture':
    case 'browser.diagnostics': {
      const session = first(snapshot?.browser?.sessions ?? []);
      if (session !== null) payload.id = session.id;
      break;
    }
    case 'queue.edit':
    case 'queue.remove':
    case 'queue.next': {
      const item = first(snapshot?.run?.queue ?? []);
      if (item !== null) payload.item = item.id;
      break;
    }
    case 'steer.send':
    case 'agent.pause':
    case 'agent.stop':
    case 'agent.model': {
      const child = (snapshot?.agents?.tree ?? []).find((node) => node.parent_id !== null) ?? null;
      if (child !== null) payload.agent = child.agent_id;
      break;
    }
    case 'change.review':
    case 'change.comment':
    case 'change.hunkAccept': {
      const file = first(snapshot?.changes?.files ?? []);
      if (file !== null) payload.path = file.path;
      break;
    }
    case 'restore.preview':
    case 'restore.run':
    case 'fork.run':
    case 'redo.run': {
      const checkpoint = first(snapshot?.checkpoints ?? []);
      if (checkpoint !== null) payload.checkpoint = checkpoint.id;
      break;
    }
    case 'session.rename':
    case 'session.pin':
    case 'session.archive':
    case 'session.delete':
    case 'session.export':
    case 'session.fork':
    case 'session.switch':
    case 'inspect':
    case 'approval.refresh':
      break;
    case 'search.titles':
      payload.query = '';
      break;
    case 'search.transcript':
      payload.query = '';
      payload.regex = false;
      payload.case_sensitive = false;
      break;
    case 'usage.filter':
      payload.cohort = 'all';
      payload.period = 'all';
      payload.worktree = '';
      break;
    case 'routing.preview':
    case 'routing.save':
      payload.mode = snapshot?.models?.routing?.mode ?? 'pinned';
      break;
    case 'model.select': {
      const value = (key: string) =>
        snapshot?.config?.values.find((entry) => entry.key === key)?.value ?? null;
      const provider = value('faktor.defaultProvider');
      const model = value('faktor.defaultModel');
      if (typeof provider === 'string') payload.provider = provider;
      if (typeof model === 'string') payload.model = model;
      break;
    }
    case 'diagnostics.lifecycle':
      payload.action = 'start';
      break;
    case 'budget.revise':
      payload.value = null;
      break;
    default:
      break;
  }
  return payload;
}

/** Commands that only read served state (no saved/verified claim). */
export const READ_COMMANDS: ReadonlySet<E5CommandKind> = new Set<E5CommandKind>([
  'inspect',
  'approval.refresh',
  'policy.audit',
  'tool.logs',
  'browser.diagnostics',
  'search.titles',
  'search.transcript',
  'usage.filter',
  'settings.search',
  'settings.export',
]);
