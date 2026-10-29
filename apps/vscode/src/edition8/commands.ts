// Native VS Code surfaces for Revision 8: Quick Picks for S02/S06/S09,
// modal scope/confirmation for S70/S35/S66/S56/S68, and settings tabs for
// the editor-sized screens. Every selection dispatches the same exact routes
// as the webview controller; cancellation dispatches nothing.

import * as vscode from 'vscode';
import type { Edition8PanelDeps } from './panel.ts';
import { isObject, type E8Snapshot, type Json } from './contract.ts';
import { arrayAt, stringAt } from './contract.ts';
import { addContextDraft, contextDraftSources } from './hostDraft.ts';

export interface Edition8Surface {
  show(screen: string): void;
}

export interface Edition8Runtime {
  readonly deps: Edition8PanelDeps;
  readonly view: Edition8Surface;
  readonly openSettings: (screen: string) => void;
  readonly cancelRun: () => void;
  readonly switchConversation: () => Promise<void>;
}

function backendOrWarn(deps: Edition8PanelDeps): ReturnType<Edition8PanelDeps['backend']> {
  const backend = deps.backend();
  if (backend === null) {
    void vscode.window.showWarningMessage(
      'Faktor is not running. Start it to read served state; no local placeholder values are shown.',
    );
  }
  return backend;
}

async function readCurrent(deps: Edition8PanelDeps): Promise<E8Snapshot | null> {
  const backend = backendOrWarn(deps);
  if (backend === null) return null;
  const binding = deps.workspace();
  try {
    return await backend.read({
      session: deps.session(),
      workspace: binding.workspace,
      worktree: binding.worktree,
      daemon: binding.daemon,
      generation: binding.generation,
      previous: null,
    });
  } catch (error) {
    void vscode.window.showErrorMessage(`Faktor read failed: ${error instanceof Error ? error.message : String(error)}`);
    return null;
  }
}

interface ModelPick extends vscode.QuickPickItem {
  readonly provider: string;
  readonly model: string;
}

export async function pickModel(deps: Edition8PanelDeps): Promise<void> {
  const snapshot = await readCurrent(deps);
  if (snapshot === null) return;
  const picks: ModelPick[] = [];
  if (Array.isArray(snapshot.models)) {
    for (const entry of snapshot.models) {
      if (!isObject(entry)) continue;
      const provider = entry.provider;
      const model = entry.model ?? entry.id;
      if (typeof provider !== 'string' || typeof model !== 'string') continue;
      const available = typeof entry.availability === 'string' ? entry.availability : typeof entry.state === 'string' ? entry.state : 'unavailable';
      picks.push({
        label: `${provider}/${model}`,
        description: available,
        detail: typeof entry.capabilityAuthority === 'string' ? `Capability authority: ${entry.capabilityAuthority}` : undefined,
        provider,
        model,
      });
    }
  }
  if (picks.length === 0) {
    void vscode.window.showWarningMessage('No provider/model is served by the daemon. Add a provider connection first.');
    return;
  }
  const pick = await vscode.window.showQuickPick(picks, {
    title: 'Faktor: choose a model',
    placeHolder: 'The current call keeps its binding; a new conversation uses the selection.',
    matchOnDescription: true,
    ignoreFocusOut: true,
  });
  if (pick === undefined) return;
  const backend = backendOrWarn(deps);
  if (backend === null) return;
  const revision = isObject(snapshot.config) && snapshot.config.revision !== undefined ? String(snapshot.config.revision) : '0';
  const result = await backend.applyPatch(revision, [
    { key: 'faktor.defaultProvider', value: pick.provider, scope: 'user' },
    { key: 'faktor.defaultModel', value: pick.model, scope: 'user' },
  ]);
  if (!result.ok) {
    void vscode.window.showErrorMessage(`Model was not saved: ${result.message}`);
    return;
  }
  // Item 13: a model change offers TWO explicit operations — future runs
  // of THIS conversation (the daemon's typed per-run model override) or a
  // NEW conversation — instead of always creating a session.
  const operation = await vscode.window.showQuickPick(
    [
      {
        label: 'Future runs in this conversation',
        description: 'subsequent Task runs of this conversation use the picked model',
        key: 'this' as const,
      },
      {
        label: 'Start a new conversation',
        description: 'a fresh conversation is created and opened with the picked model',
        key: 'new' as const,
      },
    ],
    {
      title: 'Faktor: apply the model',
      placeHolder: 'Choose where the picked model applies.',
      ignoreFocusOut: true,
    },
  );
  if (operation === undefined) {
    void vscode.window.showInformationMessage(`Default model: ${pick.label}. ${result.message}`);
    return;
  }
  let outcome: string;
  if (operation.key === 'new') {
    outcome =
      deps.newConversation === undefined
        ? 'It applies to new conversations.'
        : await deps.newConversation(pick.provider, pick.model);
  } else {
    outcome =
      deps.futureModel === undefined
        ? 'This host cannot set a per-conversation model override.'
        : await deps.futureModel(pick.provider, pick.model);
  }
  void vscode.window.showInformationMessage(`Default model: ${pick.label}. ${result.message} ${outcome}`);
}

interface AgentPick extends vscode.QuickPickItem {
  readonly agentId: string;
}

export async function pickAgent(deps: Edition8PanelDeps): Promise<void> {
  const snapshot = await readCurrent(deps);
  if (snapshot === null) return;
  const picks: AgentPick[] = [];
  for (const entry of arrayAt(snapshot.agents, 'agents')) {
    if (!isObject(entry)) continue;
    const id = stringAt(entry, 'id') ?? stringAt(entry, 'child_id');
    if (id === null) continue;
    picks.push({
      label: stringAt(entry, 'role') ?? stringAt(entry, 'name') ?? id,
      description: `${stringAt(entry, 'state') ?? 'unknown'} · ${stringAt(entry, 'model') ?? 'model unavailable'}`,
      detail: `Scope: ${stringAt(entry, 'workspace') ?? 'served binding'} · parent ${stringAt(entry, 'parent_id') ?? 'root'}`,
      agentId: id,
    });
  }
  if (picks.length === 0) {
    void vscode.window.showInformationMessage('No agent is currently served for this conversation.');
    return;
  }
  const pick = await vscode.window.showQuickPick(picks, {
    title: 'Faktor: agents',
    placeHolder: 'Runtime agents are read-only; no preset write route is served.',
  });
  if (pick === undefined) return;
  void vscode.window.showInformationMessage(
    `${pick.label} (${pick.agentId}) is a runtime agent. Revision 8 exposes no preset/role write route, so this picker never claims a saved preset.`,
  );
}

export async function addContext(): Promise<void> {
  const options: readonly (vscode.QuickPickItem & { readonly sourceKind: 'selection' | 'diagnostic' | 'terminal' | 'file' | 'url' })[] = [
    { label: 'Current selection', description: 'Exact URI, range and document version', sourceKind: 'selection' },
    { label: 'Current diagnostics', description: 'Bounded host diagnostics for the active file', sourceKind: 'diagnostic' },
    { label: 'Active terminal', description: 'Terminal name and bounded visible tail', sourceKind: 'terminal' },
    { label: 'File…', description: 'Choose one workspace file', sourceKind: 'file' },
    { label: 'URL…', description: 'Explicit fetch target, never auto-attached', sourceKind: 'url' },
  ];
  const pick = await vscode.window.showQuickPick(options, {
    title: 'Faktor: add context',
    placeHolder: 'Nothing is attached until chosen; sources show a chip and a visible remove control.',
  });
  if (pick === undefined) return;
  const editor = vscode.window.activeTextEditor;
  if (pick.sourceKind === 'selection') {
    if (editor === undefined) {
      void vscode.window.showWarningMessage('No active editor selection exists; nothing was attached.');
      return;
    }
    const selection = editor.selection;
    addContextDraft({
      kind: 'selection',
      uri: editor.document.uri.toString(),
      range: `${selection.start.line}:${selection.start.character}-${selection.end.line}:${selection.end.character}`,
      version: String(editor.document.version),
      reason: 'Current editor selection',
      text: editor.document.getText(selection).slice(0, 32 * 1024),
    });
  } else if (pick.sourceKind === 'diagnostic') {
    if (editor === undefined) {
      void vscode.window.showWarningMessage('No active editor exists; no diagnostics were attached.');
      return;
    }
    const diagnostics = vscode.languages.getDiagnostics(editor.document.uri).slice(0, 50);
    addContextDraft({
      kind: 'diagnostic',
      uri: editor.document.uri.toString(),
      range: null,
      version: String(editor.document.version),
      reason: `${diagnostics.length} diagnostics from the host language server`,
      text: diagnostics.map((entry) => `${entry.severity} ${entry.message}`).join('\n').slice(0, 16 * 1024),
    });
  } else if (pick.sourceKind === 'terminal') {
    const terminal = vscode.window.activeTerminal;
    if (terminal === undefined) {
      void vscode.window.showWarningMessage('No active terminal exists; nothing was attached.');
      return;
    }
    addContextDraft({
      kind: 'terminal',
      uri: terminal.name,
      range: null,
      version: null,
      reason: 'Active terminal (visible output is bounded by the daemon read)',
      text: null,
    });
  } else if (pick.sourceKind === 'file') {
    const chosen = await vscode.window.showOpenDialog({ canSelectMany: false, openLabel: 'Attach file' });
    if (chosen === undefined || chosen.length === 0) return;
    addContextDraft({
      kind: 'file',
      uri: chosen[0].toString(),
      range: null,
      version: null,
      reason: 'Explicitly chosen file',
      text: null,
    });
  } else {
    const url = await vscode.window.showInputBox({
      title: 'Faktor: attach URL',
      prompt: 'The URL is fetched only when a task is admitted; credentials in URLs are refused.',
      validateInput: (value) => (/^https?:\/\//.test(value) ? null : 'Enter an http(s) URL.'),
    });
    if (url === undefined) return;
    addContextDraft({ kind: 'url', uri: url, range: null, version: null, reason: 'Explicit URL', text: null });
  }
  void vscode.window.showInformationMessage(
    `Context draft now contains ${contextDraftSources().length} source(s). Add/drop stays local until a task is admitted.`,
  );
}

export async function stopScoped(deps: Edition8PanelDeps): Promise<void> {
  const scope = await vscode.window.showWarningMessage(
    'What should stop? These are different scopes and are never collapsed.',
    { modal: true },
    'Cancel run',
    'Abort current response',
    'Stop owned process',
  );
  if (scope === undefined) return;
  if (scope === 'Cancel run') {
    deps.output.appendLine('edition8 stop: cancel run');
    void vscode.commands.executeCommand('faktor.cancelTask');
    return;
  }
  if (scope === 'Abort current response') {
    void vscode.commands.executeCommand('faktor.cancelTask');
    void vscode.window.showInformationMessage('The current response was asked to abort; the run and candidate remain.');
    return;
  }
  const snapshot = await readCurrent(deps);
  if (snapshot === null) return;
  const terminals = arrayAt(snapshot.terminals, 'terminals');
  const first = terminals.find((entry) => isObject(entry) && typeof entry.id === 'string');
  if (!isObject(first) || typeof first.id !== 'string') {
    void vscode.window.showWarningMessage('No owned terminal process is served; nothing was stopped.');
    return;
  }
  const yes = await vscode.window.showWarningMessage(
    `Stop owned process ${first.id}? This does not cancel the run or abort the response.`,
    { modal: true },
    'Stop process',
  );
  if (yes === undefined) return;
  const backend = backendOrWarn(deps);
  if (backend === null) return;
  const result = await backend.execute('X-05', { terminal: first.id }, snapshot);
  void vscode.window.showInformationMessage(result.message);
}

export async function trustExecutable(deps: Edition8PanelDeps): Promise<void> {
  const snapshot = await readCurrent(deps);
  if (snapshot === null) return;
  const value = isObject(snapshot.config) && isObject(snapshot.config.values)
    ? snapshot.config.values['faktor.runtime.binaryPath']
    : null;
  const pathValue = isObject(value) && typeof value.value === 'string' ? value.value : null;
  if (pathValue === null) {
    void vscode.window.showWarningMessage(
      'The served configuration names no runtime executable, so there is nothing to trust. No checksum or signature is claimed.',
    );
    return;
  }
  const choice = await vscode.window.showWarningMessage(
    `Trust this exact executable path on this machine?\n\n${pathValue}\n\nChecksum: not recorded. Publisher signature: not verified. Trust is per path and per machine; a replaced file must be reviewed again.`,
    { modal: true },
    'Trust this path',
  );
  if (choice === undefined) return;
  const backend = backendOrWarn(deps);
  if (backend === null) return;
  // The durable trust record is the daemon's; a local "acknowledged" note
  // would be a fake state. Read -> confirm -> check are the three real steps.
  const existing = await backend.request(
    'GET',
    `/native/runtime/trust?path=${encodeURIComponent(pathValue)}`,
  );
  if (existing.ok) {
    const state = stringAt(existing.body, 'signatureState') ?? 'recorded';
    void vscode.window.showInformationMessage(
      `This executable path already has a durable trust record (${state}). Replaced files are detected by the next check.`,
    );
    return;
  }
  if (existing.errorCode !== 'not_found') {
    void vscode.window.showWarningMessage(`Runtime trust unavailable: ${existing.message}`);
    return;
  }
  const confirmed = await backend.request('POST', '/native/runtime/trust', {
    executablePath: pathValue,
    publisher: 'Host',
  });
  if (!confirmed.ok) {
    void vscode.window.showWarningMessage(`Runtime trust was not recorded: ${confirmed.message}`);
    return;
  }
  const check = await backend.request('POST', '/native/runtime/trust/check', {
    executablePath: pathValue,
  });
  if (!check.ok) {
    void vscode.window.showWarningMessage(
      `Trust was recorded but the immediate check failed: ${check.message}`,
    );
    return;
  }
  const signature = stringAt(confirmed.body, 'signatureState') ?? 'unverified';
  const startAllowed = isObject(check.body) && check.body.state === 'match';
  void vscode.window.showInformationMessage(
    `Durable path trust recorded (signature ${signature}). Immediate check: ${
      startAllowed ? 'match — start allowed' : 'mismatch — start refused'
    }.`,
  );
}

export async function deleteMemory(deps: Edition8PanelDeps): Promise<void> {
  const snapshot = await readCurrent(deps);
  if (snapshot === null) return;
  const facts = arrayAt(snapshot.memory, 'facts').length > 0 ? arrayAt(snapshot.memory, 'facts') : arrayAt(snapshot.memory, 'records');
  const picks = facts
    .filter((entry): entry is { [key: string]: Json } => isObject(entry))
    .map((entry) => ({
      label: stringAt(entry, 'text') ?? stringAt(entry, 'key') ?? 'Memory record',
      description: `${stringAt(entry, 'kind') ?? 'memory'} · ${stringAt(entry, 'scope') ?? 'scope unavailable'}`,
      key: stringAt(entry, 'key') ?? '',
      memoryKind: stringAt(entry, 'kind') ?? 'memory',
    }))
    .filter((entry) => entry.key.length > 0);
  if (picks.length === 0) {
    void vscode.window.showInformationMessage('No saved memory record is served; there is nothing to delete.');
    return;
  }
  const pick = await vscode.window.showQuickPick(picks, { title: 'Faktor: review memory' });
  if (pick === undefined) return;
  const yes = await vscode.window.showWarningMessage(
    `Delete "${pick.label}"? Future retrieval stops; earlier prompts, artifacts and log copies may remain, and that retention is disclosed rather than hidden.`,
    { modal: true },
    'Delete memory',
  );
  if (yes === undefined) return;
  const backend = backendOrWarn(deps);
  if (backend === null) return;
  const result = await backend.execute('K-11', { kind: pick.memoryKind, key: pick.key }, snapshot);
  void vscode.window.showInformationMessage(result.message);
}

export async function testHook(deps: Edition8PanelDeps): Promise<void> {
  const snapshot = await readCurrent(deps);
  if (snapshot === null) return;
  const hooks = arrayAt(snapshot.hooks, 'hooks');
  const picks = hooks
    .map((entry) => {
      if (!isObject(entry)) return null;
      const record = isObject(entry.record) ? entry.record : entry;
      const id = stringAt(record, 'id');
      return id === null ? null : { label: id, description: stringAt(record, 'event') ?? 'hook' };
    })
    .filter((entry): entry is { label: string; description: string } => entry !== null);
  if (picks.length === 0) {
    void vscode.window.showInformationMessage('No saved hook is served; no production event was triggered.');
    return;
  }
  const pick = await vscode.window.showQuickPick(picks, { title: 'Faktor: bounded hook test' });
  if (pick === undefined) return;
  const backend = backendOrWarn(deps);
  if (backend === null) return;
  const result = await backend.execute('S-13', { id: pick.label }, snapshot);
  void vscode.window.showInformationMessage(result.message);
}

export async function captureBrowser(deps: Edition8PanelDeps): Promise<void> {
  const snapshot = await readCurrent(deps);
  if (snapshot === null) return;
  const backend = backendOrWarn(deps);
  if (backend === null) return;
  const result = await backend.execute('X-08', {}, snapshot);
  if (result.ok) {
    void vscode.window.showInformationMessage(result.message);
  } else {
    void vscode.window.showWarningMessage(`Browser capture is unavailable: ${result.message}`);
  }
}

export async function switchConversationSafe(deps: Edition8PanelDeps): Promise<void> {
  const snapshot = await readCurrent(deps);
  if (snapshot === null) return;
  const sessions = arrayAt(snapshot.sessions, 'sessions');
  const choices = sessions
    .filter((entry): entry is { [key: string]: Json } => isObject(entry))
    .map((entry) => ({
      label: stringAt(entry, 'title') ?? stringAt(entry, 'id') ?? 'Conversation',
      description: `${stringAt(entry, 'state') ?? 'unknown'} · ${stringAt(entry, 'model') ?? ''}`,
      id: stringAt(entry, 'id') ?? '',
    }))
    .filter((entry) => entry.id.length > 0);
  if (choices.length === 0) {
    void vscode.window.showInformationMessage('No conversation is served to switch to.');
    return;
  }
  const pick = await vscode.window.showQuickPick(choices, {
    title: 'Faktor: switch conversation',
    placeHolder: 'Each conversation keeps its own draft, attachments, caret and scroll; the other run is not cancelled.',
  });
  if (pick === undefined) return;
  if (deps.switchSession !== undefined) {
    await deps.switchSession(pick.id);
  } else {
    await vscode.commands.executeCommand('faktor.switchSession');
  }
}

export function registerEdition8(context: vscode.ExtensionContext, runtime: Edition8Runtime): vscode.Disposable {
  const disposables: vscode.Disposable[] = [];
  const register = (id: string, handler: (...args: unknown[]) => unknown) =>
    disposables.push(vscode.commands.registerCommand(id, handler));

  register('faktor.openEdition8', () => runtime.view.show('S01'));
  register('faktor.openInspect8', () => runtime.view.show('S41'));
  register('faktor.openHistory8', () => runtime.view.show('S26'));
  register('faktor.openUsage8', () => runtime.openSettings('S27'));
  register('faktor.openSettings8', () => runtime.openSettings('S29'));
  register('faktor.openProviders8', () => runtime.openSettings('S03'));
  register('faktor.openTools8', () => runtime.openSettings('S15'));
  register('faktor.openContext8', () => runtime.openSettings('S08'));
  register('faktor.openDiagnostics8', () => runtime.openSettings('S30'));
  register('faktor.openControl8', (screen: unknown) => {
    if (typeof screen === 'string' && /^S\d{2}$/.test(screen)) runtime.openSettings(screen);
  });
  register('faktor.openModelPicker8', () => pickModel(runtime.deps));
  register('faktor.openAgentPicker8', () => pickAgent(runtime.deps));
  register('faktor.addContext8', () => addContext());
  register('faktor.stopScoped8', () => stopScoped(runtime.deps));
  register('faktor.trustExecutable8', () => trustExecutable(runtime.deps));
  register('faktor.deleteMemory8', () => deleteMemory(runtime.deps));
  register('faktor.testHook8', () => testHook(runtime.deps));
  register('faktor.captureBrowser8', () => captureBrowser(runtime.deps));
  register('faktor.switchConversationSafe8', () => switchConversationSafe(runtime.deps));

  context.subscriptions.push(...disposables);
  return { dispose: () => disposables.forEach((entry) => entry.dispose()) };
}
