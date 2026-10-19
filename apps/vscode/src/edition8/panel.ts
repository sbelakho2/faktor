// VS Code host adapter for Revision 8: one activity view with Work / Inspect
// / History, plus editor-sized settings surfaces. The webview never decides
// policy or completion; it stages typed changes and posts exact commands.

import * as vscode from 'vscode';
import { randomBytes } from 'node:crypto';
import type { E8Backend } from './client.ts';
import { renderEdition8Html } from './html.ts';
import {
  EMPTY_VIEW_CACHE,
  INITIAL_CONTROLLER_STATE,
  activateControl,
  applyDraft,
  clearDraft,
  diffView,
  restoreDefaults,
  stageRaw,
  unstage,
  type E8ControllerState,
  type E8ViewCache,
} from './host.ts';
import { E8_SCREENS } from '../generated/edition8Registry.ts';
import { E8_BINDINGS, bindingOf8 } from './bindings.ts';
import type { Json } from './contract.ts';
import { isShellScreen } from './view.ts';

export interface Edition8PanelDeps {
  readonly backend: () => E8Backend | null;
  readonly session: () => string | null;
  readonly workspace: () => { readonly workspace: string | null; readonly worktree: string | null; readonly daemon: string | null; readonly generation: number | null };
  readonly hostAction: (controlId: string) => Promise<string>;
  readonly switchSession?: (sessionId: string) => Promise<void>;
  /** Bind a picked provider/model to a new conversation; returns the copy. */
  readonly newConversation?: (provider: string, model: string) => Promise<string>;
  readonly output: vscode.OutputChannel;
}

interface PanelMessage {
  readonly type?: unknown;
  readonly controlId?: unknown;
  readonly screen?: unknown;
  readonly raw?: unknown;
  readonly useSample?: unknown;
  readonly set?: unknown;
  readonly unset?: unknown;
  readonly payload?: unknown;
}

function text(value: unknown, max = 256): string | null {
  return typeof value === 'string' && value.length > 0 && value.length <= max ? value : null;
}

const MAX_PANEL_MESSAGE_BYTES = 4 * 1024 * 1024;
const MAX_STAGED_CHANGES = 512;
const MAX_STAGED_KEY = 128;
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

function payloadOf(value: unknown): Record<string, Json> {
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

class Edition8Controller {
  public state: E8ControllerState = INITIAL_CONTROLLER_STATE;
  private cache: E8ViewCache = EMPTY_VIEW_CACHE;
  private inFlight: Promise<void> | null = null;
  private disposed = false;

  constructor(
    private readonly deps: Edition8PanelDeps,
    private readonly post: (message: unknown) => Thenable<boolean> | void,
    private readonly revealScreen: (screen: string) => void,
  ) {}

  public dispose(): void {
    this.disposed = true;
  }

  /**
   * Drop the keyed render cache so the next refresh posts a COMPLETE patch
   * set. Used when the webview document is (re)created or becomes visible:
   * a refresh that ran while the document had no listener must never leave
   * the new document showing only its initial empty markup.
   */
  public resetView(): void {
    this.cache = { controls: new Map(), rows: new Map() };
  }

  public async refresh(): Promise<void> {
    if (this.inFlight !== null) return this.inFlight;
    this.inFlight = (async () => {
      const backend = this.deps.backend();
      if (backend === null) {
        this.state = {
          ...this.state,
          loading: false,
          error: 'The Faktor service is not running. Start it to read the served state; the previous snapshot is preserved.',
        };
        this.post({ type: 'e8Error', message: this.state.error });
        this.post({ type: 'e8Header', status: 'Service unavailable', level: 'error' });
        return;
      }
      const binding = this.deps.workspace();
      this.state = { ...this.state, loading: true };
      this.post({ type: 'e8Header', status: 'Refreshing', level: 'waiting' });
      try {
        const snapshot = await backend.read({
          session: this.deps.session(),
          workspace: binding.workspace,
          worktree: binding.worktree,
          daemon: binding.daemon,
          generation: binding.generation,
          previous: this.state.snapshot,
        });
        this.state = { ...this.state, snapshot, loading: false, error: null };
        const diff = diffView(
          { snapshot, loading: false, error: null, focusControl: null },
          E8_SCREENS.map((screen) => screen.id),
          this.cache,
        );
        this.cache = diff.cache;
        // Backpressure: a refresh can carry one patch per screen. Awaiting
        // each post keeps a hidden/revealed WebviewView from dropping the
        // batch (small header/notice messages would otherwise be the only
        // ones to arrive).
        // The webview dispatches on `type`; the diff rows carry the
        // registry's `kind` discriminator. Add the wire tag explicitly so a
        // patch is never silently ignored by the renderer.
        for (const patch of diff.patches) await this.post({ type: 'e8Patch', ...patch });
        this.post({
          type: 'e8Header',
          status: snapshot.stale ? 'Stale' : 'Ready',
          level: snapshot.stale ? 'waiting' : 'quiet',
        });
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        this.deps.output.appendLine(`edition8 refresh failed: ${message}`);
        this.state = { ...this.state, loading: false, error: message };
        this.post({ type: 'e8Error', message });
        this.post({ type: 'e8Header', status: 'Refresh failed', level: 'error' });
      } finally {
        this.inFlight = null;
      }
    })();
    return this.inFlight;
  }

  public async handle(message: PanelMessage): Promise<void> {
    if (this.disposed) return;
    if (!messageWithinBound(message)) return;
    const type = text(message.type, 64);
    if (type === null) return;
    switch (type) {
      case 'e8Ready':
        // The webview script only starts listening now. Any refresh that ran
        // at view/panel resolve was posted into a document with no listener
        // (or into the previous document after a re-render); its cache must
        // not suppress the first complete patch set. Clearing it makes the
        // ready handshake re-render every served row/control.
        this.resetView();
        await this.refresh();
        return;
      case 'e8Refresh':
        // An explicit refresh is also the recovery path for a document that
        // missed earlier posts (hidden reveal, webview reload). Re-render the
        // complete served state instead of trusting a cache the document
        // never received.
        this.resetView();
        await this.refresh();
        this.post({ type: 'e8Notice', message: 'Served state refreshed from the daemon.' });
        return;
      case 'e8SelectScreen': {
        const screen = text(message.screen, 16);
        if (screen !== null && E8_SCREENS.some((entry) => entry.id === screen)) {
          this.revealScreen(screen);
        }
        return;
      }
      case 'e8OpenScreen': {
        const screen = text(message.screen, 16);
        if (screen !== null && E8_SCREENS.some((entry) => entry.id === screen)) {
          await vscode.commands.executeCommand('faktor.openControl8', screen);
        }
        return;
      }
      case 'e8Stage': {
        const controlId = text(message.controlId, 16);
        if (controlId === null || !E8_BINDINGS.has(controlId)) return;
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
          this.post({ type: 'e8Draft', error: staged.error });
          return;
        }
        this.state = staged.state;
        this.post({
          type: 'e8Draft',
          set: [...this.state.draft.set.values()],
          unset: [...this.state.draft.unset],
          notice: `${controlId} staged locally.`,
        });
        return;
      }
      case 'e8Unstage': {
        const controlId = text(message.controlId, 16);
        if (controlId === null || !E8_BINDINGS.has(controlId)) return;
        this.state = unstage(this.state, controlId);
        this.post({ type: 'e8Draft', set: [...this.state.draft.set.values()], unset: [...this.state.draft.unset] });
        return;
      }
      case 'e8Reset':
        this.state = clearDraft(this.state);
        this.post({ type: 'e8Draft', set: [], unset: [], notice: 'The staged draft was cleared; applied values are unchanged.' });
        return;
      case 'e8Defaults':
        this.state = restoreDefaults(this.state);
        this.post({
          type: 'e8Draft',
          set: [],
          unset: [...this.state.draft.unset],
          notice: 'Defaults are staged as removals until Apply; nothing was written yet.',
        });
        return;
      case 'e8Apply': {
        const backend = this.deps.backend();
        if (backend === null) {
          this.post({ type: 'e8Error', message: 'The Faktor service is not running; the draft is preserved.' });
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
                  typeof entry === 'string' && entry.length <= 64 && E8_BINDINGS.has(entry),
              )
          : [...this.state.draft.unset];
        const outcome = await applyDraft(backend, this.state, set, unset);
        this.state = outcome.state;
        this.post({ type: 'e8ActionResult', ok: outcome.ok, message: outcome.message });
        this.post({ type: 'e8Draft', set: [...this.state.draft.set.values()], unset: [...this.state.draft.unset] });
        await this.refresh();
        return;
      }
      case 'e8Activate': {
        const backend = this.deps.backend();
        const controlId = text(message.controlId, 16);
        if (backend === null || controlId === null || !E8_BINDINGS.has(controlId)) {
          this.post({ type: 'e8Error', message: 'The Faktor service is not running; no control was dispatched.' });
          return;
        }
        const binding = bindingOf8(controlId);
        const payload: Record<string, Json> = { ...payloadOf(message.payload) };
        if (this.state.snapshot?.binding?.session !== null && payload.session === undefined) {
          payload.session = this.state.snapshot?.binding?.session ?? null;
        }
        if (binding.configKey !== null && payload.key === undefined) payload.key = binding.configKey;
        if (binding.route.path.includes(':id') && payload.id === undefined) {
          const first = Array.isArray((this.state.snapshot?.connections as { connections?: unknown })?.connections)
            ? ((this.state.snapshot?.connections as { connections?: { id?: string }[] }).connections ?? []).find((entry) => typeof entry.id === 'string')
            : undefined;
          if (first?.id !== undefined) payload.id = first.id;
        }
        const activated = await activateControl(backend, this.state, controlId, payload);
        this.state = activated.state;
        if (activated.result.kind === 'host') {
          const messageText = await this.deps.hostAction(controlId);
          this.post({ type: 'e8ActionResult', ok: true, message: messageText });
        } else if (activated.result.kind === 'staged') {
          this.post({
            type: 'e8Draft',
            set: [...this.state.draft.set.values()],
            unset: [...this.state.draft.unset],
            notice: activated.result.message,
          });
        } else {
          this.post({
            type: 'e8ActionResult',
            ok: activated.result.error === null,
            message: activated.result.message,
          });
        }
        await this.refresh();
        return;
      }
      default:
        return;
    }
  }
}

function isStagedChange(value: unknown): value is { controlId: string; key: string; value: Json; scope: string } {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return false;
  const record = value as Record<string, unknown>;
  if (
    typeof record.controlId !== 'string' ||
    record.controlId.length === 0 ||
    record.controlId.length > 16 ||
    typeof record.key !== 'string' ||
    record.key.length === 0 ||
    record.key.length > MAX_STAGED_KEY ||
    typeof record.scope !== 'string' ||
    record.scope !== 'user'
  ) {
    return false;
  }
  const binding = E8_BINDINGS.get(record.controlId);
  if (binding === undefined || binding.configKey !== record.key) return false;
  const staged = record.value;
  return (
    staged === null ||
    typeof staged === 'boolean' ||
    (typeof staged === 'number' && Number.isFinite(staged)) ||
    (typeof staged === 'string' && staged.length <= MAX_STAGED_STRING)
  );
}

export class Edition8Panel {
  public static current: Edition8Panel | null = null;
  private static readonly viewType = 'faktor.edition8.settings';
  private readonly controller: Edition8Controller;

  public static createOrShow(context: vscode.ExtensionContext, deps: Edition8PanelDeps, screen = 'S29'): Edition8Panel {
    const column = vscode.ViewColumn.One;
    if (Edition8Panel.current !== null) {
      Edition8Panel.current.panel.reveal(column);
      Edition8Panel.current.controller.handle({ type: 'e8SelectScreen', screen });
      return Edition8Panel.current;
    }
    const panel = vscode.window.createWebviewPanel(
      Edition8Panel.viewType,
      'Faktor Settings',
      column,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [vscode.Uri.joinPath(context.extensionUri, 'media')],
      },
    );
    Edition8Panel.current = new Edition8Panel(panel, context, deps, screen);
    return Edition8Panel.current;
  }

  private constructor(
    private readonly panel: vscode.WebviewPanel,
    private readonly context: vscode.ExtensionContext,
    deps: Edition8PanelDeps,
    screen: string,
  ) {
    const initial = E8_SCREENS.some((entry) => entry.id === screen) ? screen : 'S29';
    this.controller = new Edition8Controller(deps, (message) => {
      return this.panel.webview.postMessage(message);
    }, (next) => {
      // Selection is a client-side view switch; re-rendering the whole
      // document would discard staged drafts and input focus.
      void next;
    });
    // Register the message handler BEFORE handing the document to the
    // webview: the script posts `e8Ready` as soon as it runs, and a gap here
    // dropped that handshake (the panel then kept its initial empty rows).
    panel.webview.onDidReceiveMessage((message: PanelMessage) => {
      void this.controller.handle(message);
    });
    panel.webview.html = this.render(initial);
    panel.onDidDispose(() => {
      this.controller.dispose();
      if (Edition8Panel.current === this) Edition8Panel.current = null;
    });
  }

  private render(screen: string): string {
    const webview = this.panel.webview;
    const nonce = randomBytes(16).toString('hex');
    const styleUri = webview.asWebviewUri(vscode.Uri.joinPath(this.context.extensionUri, 'media', 'edition8.css'));
    const scriptUri = webview.asWebviewUri(vscode.Uri.joinPath(this.context.extensionUri, 'media', 'edition8.js'));
    return renderEdition8Html({
      nonce,
      cspSource: webview.cspSource,
      scriptUri: scriptUri.toString(),
      styleUri: styleUri.toString(),
      state: { snapshot: this.controller.state.snapshot, loading: this.controller.state.loading, error: this.controller.state.error, focusControl: null },
      initialScreen: screen,
      host: 'vscode',
      mode: 'settings',
    });
  }
}

export class Edition8ViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = 'faktor.edition8';
  private view: vscode.WebviewView | null = null;
  private controller: Edition8Controller | null = null;
  private initialScreen = 'S01';

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly deps: Edition8PanelDeps,
  ) {}

  public resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.context.extensionUri, 'media')],
    };
    this.controller = new Edition8Controller(this.deps, (message) => {
      return view.webview.postMessage(message);
    }, (screen) => {
      this.initialScreen = screen;
    });
    // Handler before HTML: `e8Ready` is posted by the script immediately.
    view.webview.onDidReceiveMessage((message: PanelMessage) => {
      void this.controller?.handle(message);
    });
    view.webview.html = this.render(this.initialScreen);
    // A WebviewView can be resolved while hidden; posts to a hidden document
    // are not a durable replay buffer. On every reveal, invalidate the keyed
    // cache so the visible document receives the complete served state (the
    // snapshot itself is retained and never re-read here).
    view.onDidChangeVisibility(() => {
      if (view.visible !== true) return;
      this.controller?.resetView();
      void this.controller?.refresh();
    });
  }

  public show(screen: string): void {
    if (screen !== 'S01') this.initialScreen = screen;
    if (this.view !== null) {
      this.view.show?.(true);
      this.view.webview.html = this.render(this.initialScreen);
      void this.controller?.refresh();
    } else {
      void vscode.commands.executeCommand('faktor.edition8.focus');
    }
  }

  private render(screen: string): string {
    if (this.view === null) return '';
    const webview = this.view.webview;
    const nonce = randomBytes(16).toString('hex');
    const styleUri = webview.asWebviewUri(vscode.Uri.joinPath(this.context.extensionUri, 'media', 'edition8.css'));
    const scriptUri = webview.asWebviewUri(vscode.Uri.joinPath(this.context.extensionUri, 'media', 'edition8.js'));
    return renderEdition8Html({
      nonce,
      cspSource: webview.cspSource,
      scriptUri: scriptUri.toString(),
      styleUri: styleUri.toString(),
      state: {
        snapshot: this.controller?.state.snapshot ?? null,
        loading: this.controller?.state.loading ?? false,
        error: this.controller?.state.error ?? null,
        focusControl: null,
      },
      initialScreen: isShellScreen(screen) ? screen : 'S01',
      host: 'vscode',
      mode: 'shell',
    });
  }
}
