// The Faktor Control Center editor panel. One webview, one typed daemon
// client, one atomic settings draft. The panel owns no policy: it routes
// typed commands, re-reads served state and sends the minimum view delta.

import * as vscode from 'vscode';
import { randomBytes } from 'node:crypto';
import type { E5Backend } from './client.ts';
import { renderEdition5Html } from './html.ts';
import {
  EMPTY_VIEW_CACHE,
  INITIAL_CONTROLLER_STATE,
  activateControl,
  applyDraft,
  diffView,
  restoreDefaults,
  stageRaw,
  unstage,
  type E5ControllerState,
  type E5ViewCache,
} from './host.ts';
import { E5_SCREENS } from '../generated/edition5Registry.ts';
import { E5_BINDINGS } from './bindings.ts';
import { validateMcpPayload, validateSteerText, type E5Payload } from './commands.ts';
import type { Json } from './contract.ts';

export interface Edition5PanelDeps {
  /** The live daemon backend, or null when the daemon is not running. */
  readonly backend: () => E5Backend | null;
  /** The bound session id (null before the first conversation). */
  readonly session: () => string | null;
  /** Native VS Code action invoked by a host.action control. */
  readonly hostAction: (action: string) => Promise<string>;
  readonly output: vscode.OutputChannel;
}

interface PanelMessage {
  readonly type?: unknown;
  readonly controlId?: unknown;
  readonly command?: unknown;
  readonly raw?: unknown;
  readonly useSample?: unknown;
  readonly set?: unknown;
  readonly unset?: unknown;
  readonly payload?: unknown;
  readonly screen?: unknown;
}

function text(value: unknown, max = 256): string | null {
  return typeof value === 'string' && value.length > 0 && value.length <= max ? value : null;
}

const MAX_PANEL_MESSAGE_BYTES = 4 * 1024 * 1024;
const MAX_STAGED_CHANGES = 512;
const MAX_STAGED_STRING = 256 * 1024;
const MAX_PAYLOAD_KEYS = 64;
const MAX_PAYLOAD_ARRAY = 256;

function messageWithinBound(message: unknown): boolean {
  try {
    const encoded = JSON.stringify(message);
    return typeof encoded === 'string' && encoded.length <= MAX_PANEL_MESSAGE_BYTES;
  } catch {
    return false;
  }
}

function payloadOf(value: unknown): E5Payload {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return {};
  const out: Record<string, Json> = {};
  const entries = Object.entries(value as Record<string, unknown>).slice(0, MAX_PAYLOAD_KEYS);
  for (const [key, entry] of entries) {
    if (key.length === 0 || key.length > 128) continue;
    if (entry === null || ['string', 'number', 'boolean'].includes(typeof entry)) {
      if (typeof entry === 'string' && entry.length > MAX_STAGED_STRING) continue;
      out[key] = entry as Json;
    } else if (
      Array.isArray(entry) &&
      entry.length <= MAX_PAYLOAD_ARRAY &&
      entry.every((item) => typeof item === 'string' && item.length <= MAX_STAGED_STRING)
    ) {
      out[key] = entry as string[];
    }
  }
  return out;
}

export class Edition5Panel {
  public static current: Edition5Panel | null = null;
  private static readonly viewType = 'faktor.controlCenter';

  private state: E5ControllerState = INITIAL_CONTROLLER_STATE;
  private cache: E5ViewCache = EMPTY_VIEW_CACHE;
  private selectedScreen = 'S01';
  private refreshInFlight: Promise<void> | null = null;
  private disposed = false;

  public static createOrShow(context: vscode.ExtensionContext, deps: Edition5PanelDeps, screen = 'S01'): Edition5Panel {
    const column = vscode.ViewColumn.One;
    if (Edition5Panel.current !== null) {
      Edition5Panel.current.panel.reveal(column);
      void Edition5Panel.current.selectScreen(screen);
      return Edition5Panel.current;
    }
    const panel = vscode.window.createWebviewPanel(
      Edition5Panel.viewType,
      'Faktor Control Center',
      column,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [vscode.Uri.joinPath(context.extensionUri, 'media')],
      },
    );
    Edition5Panel.current = new Edition5Panel(panel, context, deps, screen);
    return Edition5Panel.current;
  }

  private constructor(
    private readonly panel: vscode.WebviewPanel,
    private readonly context: vscode.ExtensionContext,
    private readonly deps: Edition5PanelDeps,
    screen: string,
  ) {
    this.selectedScreen = E5_SCREENS.some((entry) => entry.id === screen) ? screen : 'S01';
    panel.webview.html = this.render();
    panel.webview.onDidReceiveMessage((message: PanelMessage) => {
      void this.handle(message);
    });
    panel.onDidDispose(() => {
      this.disposed = true;
      if (Edition5Panel.current === this) Edition5Panel.current = null;
    });
    void this.refresh();
  }

  public reveal(screen?: string): void {
    this.panel.reveal(vscode.ViewColumn.One);
    if (screen !== undefined) void this.selectScreen(screen);
  }

  private render(): string {
    const webview = this.panel.webview;
    const nonce = randomBytes(16).toString('hex');
    const styleUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this.context.extensionUri, 'media', 'edition5.css'),
    );
    const scriptUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this.context.extensionUri, 'media', 'edition5.js'),
    );
    return renderEdition5Html({
      nonce,
      cspSource: webview.cspSource,
      scriptUri: scriptUri.toString(),
      styleUri: styleUri.toString(),
      state: { ...this.state, snapshot: this.state.snapshot, focusControl: null },
      initialScreen: this.selectedScreen,
      host: 'vscode',
    });
  }

  private post(message: unknown): void {
    if (!this.disposed) void this.panel.webview.postMessage(message);
  }

  private postDraft(error: string | null = null, notice: string | null = null): void {
    this.post({
      type: 'e5Draft',
      set: [...this.state.draft.set.values()],
      unset: [...this.state.draft.unset],
      error,
      notice,
    });
  }

  private async selectScreen(screen: string): Promise<void> {
    if (!E5_SCREENS.some((entry) => entry.id === screen)) return;
    this.selectedScreen = screen;
    this.post({ type: 'e5SelectScreen', screen });
    await this.refresh();
  }

  private async refresh(): Promise<void> {
    if (this.refreshInFlight !== null) return this.refreshInFlight;
    this.refreshInFlight = (async () => {
      const backend = this.deps.backend();
      const session = this.deps.session();
      try {
        if (backend === null) {
          this.state = {
            ...this.state,
            loading: false,
            error: 'The Faktor service is not running. Start it to read the Edition 5 state.',
          };
          this.post({
            type: 'e5Error',
            message: this.state.error,
          });
          return;
        }
        this.state = { ...this.state, loading: true };
        const snapshot = await backend.read(session);
        this.state = { ...this.state, snapshot, loading: false, error: null };
        const diff = diffView(snapshot, this.cache, 'Edition 5 state refreshed from the daemon.');
        this.cache = diff.cache;
        this.post({ type: 'e5Patch', ...diff.patch });
        if (this.state.draft.set.size > 0 || this.state.draft.unset.size > 0) {
          this.postDraft();
        }
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        this.deps.output.appendLine(`edition5 refresh failed: ${message}`);
        this.state = { ...this.state, loading: false, error: message };
        this.post({ type: 'e5Error', message });
      } finally {
        this.refreshInFlight = null;
      }
    })();
    return this.refreshInFlight;
  }

  private async handle(message: PanelMessage): Promise<void> {
    if (!messageWithinBound(message)) return;
    const type = text(message.type, 64);
    if (type === null) return;
    switch (type) {
      case 'e5Ready':
        await this.refresh();
        return;
      case 'e5SelectScreen': {
        const screen = text(message.screen, 16);
        if (screen !== null) await this.selectScreen(screen);
        return;
      }
      case 'e5Inspect': {
        const controlId = text(message.controlId, 16);
        if (controlId !== null) {
          this.post({ type: 'e5Notice', message: `Inspecting ${controlId}; its contract and served state are in the card.` });
        }
        return;
      }
      case 'e5Stage': {
        const controlId = text(message.controlId, 16);
        if (controlId === null || !E5_BINDINGS.has(controlId)) return;
        const raw =
          typeof message.raw === 'string' && message.raw.length <= MAX_STAGED_STRING
            ? message.raw
            : typeof message.raw === 'boolean'
              ? message.raw
              : typeof message.raw === 'number' && Number.isFinite(message.raw)
                ? message.raw
                : null;
        const staged = stageRaw(this.state, controlId, raw, message.useSample === true);
        if (staged.error !== null) {
          this.postDraft(staged.error);
          return;
        }
        this.state = staged.state;
        this.postDraft(null, `${controlId} staged locally.`);
        return;
      }
      case 'e5Unstage': {
        const controlId = text(message.controlId, 16);
        if (controlId === null || !E5_BINDINGS.has(controlId)) return;
        this.state = unstage(this.state, controlId);
        this.postDraft();
        return;
      }
      case 'e5Reset':
        this.state = { ...this.state, draft: { set: new Map(), unset: new Set() } };
        this.postDraft(null, 'The staged draft was reset to the applied values.');
        return;
      case 'e5Defaults':
        this.state = restoreDefaults(this.state);
        this.postDraft(null, 'Defaults are staged as removals until Apply; nothing was written yet.');
        return;
      case 'e5Apply': {
        const backend = this.deps.backend();
        if (backend === null) {
          this.post({ type: 'e5Error', message: 'The Faktor service is not running; the draft is preserved.' });
          return;
        }
        const set = Array.isArray(message.set)
          ? (message.set as unknown[]).slice(0, MAX_STAGED_CHANGES).filter(isStagedChange)
          : [...this.state.draft.set.values()];
        const unset = Array.isArray(message.unset)
          ? (message.unset as unknown[])
              .slice(0, MAX_STAGED_CHANGES)
              .filter(
                (entry): entry is string =>
                  typeof entry === 'string' && entry.length <= 64 && E5_BINDINGS.has(entry),
              )
          : [...this.state.draft.unset];
        const outcome = await applyDraft(backend, this.state, set, unset);
        this.state = outcome.state;
        if (outcome.patch !== null) {
          const diff = diffView(this.state.snapshot, this.cache, outcome.message);
          this.cache = diff.cache;
          this.post({ type: 'e5Patch', ...diff.patch });
        }
        this.post({
          type: 'e5ActionResult',
          ok: outcome.ok,
          message: outcome.message,
        });
        this.postDraft(outcome.ok ? null : outcome.message);
        return;
      }
      case 'e5Activate': {
        const backend = this.deps.backend();
        const controlId = text(message.controlId, 16);
        if (backend === null || controlId === null || !E5_BINDINGS.has(controlId)) {
          this.post({ type: 'e5Error', message: 'The Faktor service is not running; no control was dispatched.' });
          return;
        }
        const payload: Record<string, Json> = { ...payloadOf(message.payload) };
        if (message.payload !== null && typeof message.payload === 'object' && !Array.isArray(message.payload)) {
          const value = (message.payload as Record<string, unknown>).value;
          if (typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean') {
            payload.value = value;
          }
        }
        const activated = await activateControl(backend, this.state, controlId, payload);
        this.state = activated.state;
        if (activated.result.kind === 'host') {
          const result = await this.deps.hostAction(activated.result.controlId);
          this.post({ type: 'e5ActionResult', ok: true, message: result });
        } else if (activated.result.kind === 'staged') {
          this.postDraft(null, activated.result.message);
        } else {
          this.post({
            type: 'e5ActionResult',
            ok: activated.result.error === null,
            message: activated.result.message,
          });
          if (activated.result.error !== null) this.postDraft(activated.result.error);
        }
        const diff = diffView(this.state.snapshot, this.cache, activated.result.message);
        this.cache = diff.cache;
        this.post({ type: 'e5Patch', ...diff.patch });
        return;
      }
      case 'e5Action': {
        const backend = this.deps.backend();
        const command = text(message.command, 64);
        if (backend === null || command === null) {
          this.post({ type: 'e5Error', message: 'The Faktor service is not running; the action was not sent.' });
          return;
        }
        const payload: Record<string, Json> = { ...payloadOf(message.payload) };
        if (this.state.snapshot?.binding?.session != null && payload.session === undefined) {
          payload.session = this.state.snapshot.binding.session;
        }
        if (command === 'mcp.create') {
          const refusal = validateMcpPayload(payload, this.state.snapshot?.tools?.mcp.length ?? 0);
          if (refusal !== null) {
            this.post({ type: 'e5ActionResult', ok: false, message: refusal });
            return;
          }
        }
        if (command === 'steer.send') {
          const refusal = validateSteerText(payload.text);
          if (refusal !== null) {
            this.post({ type: 'e5ActionResult', ok: false, message: refusal });
            return;
          }
        }
        const action = await backend.execute(command as import('./bindings.ts').E5CommandKind, payload);
        const diff = diffView(this.state.snapshot, this.cache, action.message);
        this.cache = diff.cache;
        if (action.ok) {
          try {
            const snapshot = await backend.read(this.deps.session());
            this.state = { ...this.state, snapshot, error: null };
            const next = diffView(snapshot, this.cache, action.message);
            this.cache = next.cache;
            this.post({ type: 'e5Patch', ...next.patch });
          } catch (error) {
            this.post({ type: 'e5Notice', message: error instanceof Error ? error.message : String(error) });
          }
        }
        this.post({ type: 'e5ActionResult', ok: action.ok, message: action.message });
        this.postDraft(action.ok ? null : action.message);
        return;
      }
      default:
        return;
    }
  }
}

function isStagedChange(value: unknown): value is { controlId: string; key: string; value: Json } {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return false;
  const record = value as Record<string, unknown>;
  if (
    typeof record.controlId !== 'string' ||
    record.controlId.length === 0 ||
    record.controlId.length > 16 ||
    typeof record.key !== 'string' ||
    record.key.length === 0 ||
    record.key.length > 128
  ) {
    return false;
  }
  const binding = E5_BINDINGS.get(record.controlId);
  if (binding === undefined || binding.configKey !== record.key) return false;
  const staged = record.value;
  return (
    staged === null ||
    typeof staged === 'boolean' ||
    (typeof staged === 'number' && Number.isFinite(staged)) ||
    (typeof staged === 'string' && staged.length <= MAX_STAGED_STRING)
  );
}
