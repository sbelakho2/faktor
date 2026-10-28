// The Faktor chat webview: ONE Faktor-owned surface (`media/chat.js` +
// `media/chat.css` + `media/composer-state.js`, all hand-written and
// dependency-free). There is no vendored upstream bundle, no message-ABI
// bridge and no checkout-relative lookup: the panel ships inside the VSIX
// and every message is a bounded, Faktor-native `ChatMessage` the extension
// host routes itself.
//
// This module owns ONLY VS Code webview plumbing — HTML generation, CSP,
// message transport — and delegates every daemon action to an injected host
// (extension.ts). No remote scripts, no eval, no inline handlers;
// `media/chat.js` is the only script, and every daemon-derived string is
// rendered as text.

import * as vscode from 'vscode';
import { randomBytes } from 'node:crypto';
import type { FaktorSnapshot } from './state';
import type { UserErrorProjection } from './errors';
import { renderEdition8AppMarkup } from './edition8/html';
import {
  Edition8Controller,
  type Edition8PanelDeps,
  type PanelMessage,
} from './edition8/panel';

/** Messages the webview sends to the extension host. */
export interface ChatMessage {
  readonly type: string;
  readonly [key: string]: unknown;
}

/**
 * The composer can carry up to 32 MiB of raw attachment bytes (base64 on
 * the wire), so the envelope bound sits above that contract; every handler
 * still applies its own tighter field/count/byte caps.
 */
const MAX_WEBVIEW_MESSAGE_BYTES = 64 * 1024 * 1024;

/**
 * Validate the host<->webview envelope before dispatch: an object (not an
 * array/null), a non-empty bounded `type`, and a bounded serialized size.
 * Untrusted webview content cannot hand the host a multi-gigabyte nested
 * value, a prototype-shaped non-object, or an unbounded command name.
 */
/**
 * The installed extension's own version, used for webview asset cache-busting
 * and the header badge. Reading it at runtime (instead of a hardcoded
 * literal) makes every release bust the webview cache automatically — a
 * stale `?v=` pin silently served an OLD chat.js across upgrades.
 */
function assetVersion(): string {
  const version = vscode.extensions.getExtension('faktor.faktor')?.packageJSON
    ?.version as string | undefined;
  return typeof version === 'string' && version.length > 0 ? version : '0.0.0-dev';
}

function validChatMessage(value: unknown): value is ChatMessage {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return false;
  const type = (value as { type?: unknown }).type;
  if (typeof type !== 'string' || type.length === 0 || type.length > 64) return false;
  try {
    const encoded = JSON.stringify(value);
    return typeof encoded === 'string' && encoded.length <= MAX_WEBVIEW_MESSAGE_BYTES;
  } catch {
    return false;
  }
}

/** The extension-side handler the provider delegates every message to. */
export interface ChatHost {
  handle(message: ChatMessage): void | Promise<void>;
  /**
   * The view was (re)resolved. Host-owned state that is NOT carried by the
   * snapshot (composer attachment metadata, the current stream-block reason)
   * must be re-posted here: a disposed/reopened view starts with an empty
   * panel, and the host-side bytes/refusals would otherwise stay invisible.
   */
  onViewResolved?(): void | Promise<void>;
}

export type FaktorView = 'work' | 'settings' | 'inspect' | 'history' | 'diagnostics';

/**
 * The ONE Faktor surface. It is the primary activity-bar webview view
 * (`faktor.launcher`): a full-height sidebar conversation, never an editor
 * tab by default. Chat, Settings, Inspect, History and Diagnostics are
 * internal destinations of this same webview: the host never creates a
 * panel per surface and never splits an editor column.
 */
export class ChatViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = 'faktor.chat';

  private view: vscode.WebviewView | null = null;
  private edition8Controller: Edition8Controller | null = null;
  private snapshot: FaktorSnapshot | null = null;
  private initialScreen = 'S29';
  private pendingView: FaktorView | null = null;
  private pendingSettings: { readonly screen: string; readonly home: boolean } | null = null;
  /**
   * One pending full snapshot. The store can emit several patches in a burst
   * (session patches, task patches, transcript patch); a newer full snapshot
   * supersedes all of them, so the webview receives at most one frame per
   * flush window instead of one per patch. Non-snapshot messages are posted
   * immediately and stay ordered behind the flushed snapshot because the
   * webview applies the queue in order.
   */
  private pendingSnapshot: FaktorSnapshot | null = null;
  private flushTimer: NodeJS.Timeout | null = null;

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly host: ChatHost,
    private readonly edition8: (() => Edition8PanelDeps) | null = null,
  ) {}

  private get extensionUri(): vscode.Uri {
    return this.context.extensionUri;
  }

  /**
   * VS Code resolves the activity-bar view here. The view is the primary
   * surface: it renders the chat document and owns its message transport.
   * A hidden/re-created view re-posts the last snapshot and replays any
   * pending internal destination after the webview's `ready` handshake.
   */
  public resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.extensionUri, 'media')],
    };
    this.edition8Controller?.dispose();
    const deps = this.edition8 === null ? null : this.edition8();
    this.edition8Controller =
      deps === null ? null : new Edition8Controller(deps, (message) => this.post(message));
    // Register the handler before the document so the `ready`/`e8Ready`
    // handshakes cannot fall into a gap.
    view.webview.onDidReceiveMessage((message: unknown) => this.handleMessage(message));
    view.webview.html = this.render(view.webview);
    view.onDidDispose(() => {
      this.edition8Controller?.dispose();
      this.edition8Controller = null;
      this.view = null;
      if (this.flushTimer !== null) {
        clearTimeout(this.flushTimer);
        this.flushTimer = null;
      }
      this.pendingSnapshot = null;
    });
    if (this.snapshot !== null) {
      this.post({ type: 'snapshot', snapshot: this.snapshot });
    }
    void this.host.onViewResolved?.();
    if (this.pendingView === 'settings') this.refreshSettings();
  }

  private handleMessage(message: unknown): void {
    if (!validChatMessage(message)) return;
    if (message.type.startsWith('e8')) {
      if (message.type === 'e8Ready' && this.pendingSettings !== null) {
        this.post(
          this.pendingSettings.home
            ? { type: 'e8SettingsHome' }
            : { type: 'e8SelectScreen', screen: this.pendingSettings.screen },
        );
      }
      void this.edition8Controller?.handle(message as PanelMessage);
      return;
    }
    if (message.type === 'openSettingsView') {
      this.showSettings('S29');
      return;
    }
    if (message.type === 'ready') {
      if (this.pendingView !== null) {
        this.post({ type: 'faktorShowView', view: this.pendingView });
      }
      void this.host.onViewResolved?.();
    }
    void this.host.handle(message);
  }

  /** Reveal the primary sidebar view; never create an editor tab. */
  private reveal(): void {
    if (this.view !== null) {
      this.view.show(true);
      return;
    }
    void vscode.commands.executeCommand(`${ChatViewProvider.viewType}.focus`);
  }

  /** `Open` and the activity bar focus the one view on Chat. */
  focus(): void {
    this.pendingView = 'work';
    this.reveal();
    this.post({ type: 'faktorShowView', view: 'work' });
    void this.host.onViewResolved?.();
  }

  /** Switch the one view to an internal destination. */
  showView(view: FaktorView): void {
    this.pendingView = view;
    this.reveal();
    if (view === 'settings') {
      this.pendingSettings = { screen: 'S29', home: true };
      this.post({ type: 'e8SettingsHome' });
    }
    this.post({ type: 'faktorShowView', view });
    if (view === 'settings') this.refreshSettings();
  }

  /**
   * Open Settings inside the same view. `home` selects the one settings
   * descriptor surface; otherwise the requested registry screen is selected.
   */
  showSettings(screen = 'S29', home = true): void {
    this.pendingView = 'settings';
    this.pendingSettings = { screen, home };
    this.reveal();
    this.post({ type: 'faktorShowView', view: 'settings' });
    if (home) {
      this.post({ type: 'e8SettingsHome' });
    } else {
      this.post({ type: 'e8SelectScreen', screen });
    }
    this.refreshSettings();
  }

  /** Map an Edition 8 command entry point onto the shared internal views. */
  showEdition8Screen(screen: string): void {
    if (screen === 'S01') {
      this.showView('work');
    } else if (screen === 'S41') {
      this.showView('inspect');
    } else if (screen === 'S26') {
      this.showView('history');
    } else {
      this.showSettings(screen, false);
    }
  }

  private refreshSettings(): void {
    this.edition8Controller?.resetView();
    void this.edition8Controller?.refresh();
  }

  /** Push a full state snapshot; the webview re-renders from it. */
  postSnapshot(snapshot: FaktorSnapshot): void {
    this.snapshot = snapshot;
    this.pendingSnapshot = snapshot;
    if (this.flushTimer !== null) {
      return;
    }
    this.flushTimer = setTimeout(() => this.flushPendingSnapshot(), 16);
  }

  /**
   * Deliver the newest queued snapshot NOW. Every non-snapshot message calls
   * this first: a result/evidence/notice must never overtake the state it was
   * derived from, and a stale snapshot must never land after (and visually
   * undo) a newer result.
   */
  private flushPendingSnapshot(): void {
    if (this.flushTimer !== null) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    const next = this.pendingSnapshot;
    this.pendingSnapshot = null;
    if (next !== null) {
      void this.view?.webview.postMessage({ type: 'snapshot', snapshot: next });
    }
  }

  /** Deliver the decoded bytes of one expanded evidence artifact. */
  postEvidence(id: number, text: string, truncated: boolean): void {
    this.post({ type: 'evidence', id, text, truncated });
  }

  /**
   * The result of one selected-change review (I03): `opened` names the
   * served evidence id whose immutable document (or side-by-side diff) the
   * native editor now shows; `unavailable` asks the Changes view to render
   * its exact `Diff unavailable` state (no live workspace read happened).
   */
  postChangeReview(result: {
    readonly path: string;
    readonly state: 'opened' | 'unavailable';
    readonly evidenceId?: number;
    readonly reason?: string | null;
  }): void {
    this.post({ type: 'changeReview', ...result });
  }

  /**
   * The result of one `sendGoal`: the built-in composer clears its draft
   * ONLY when `ok` is true and the textarea still holds the submitted goal
   * (see media/composer-state.js). A failed result carries its `retryable`
   * classification: only a lost/unavailable start response reuses the SAME
   * logical submission on retry (T08 `Start response unavailable`); a typed
   * local refusal ends the submission and keeps only the draft.
   */
  postStartResult(goal: string, ok: boolean, retryable = false): void {
    this.post(
      ok ? { type: 'startResult', goal, ok: true } : { type: 'startResult', goal, ok: false, retryable },
    );
  }

  /**
   * Restore one failed pending submission in the composer: the original
   * text travels back verbatim through `startResult` with `ok: false`, so
   * the draft (and its completion contract) is kept for the retry and never
   * silently lost. The failure message itself was already surfaced through
   * `postNotice`. `retryable` mirrors the host gate: a transport-class loss
   * keeps the logical submission id, a typed refusal ends it.
   */
  postSendMessageFailed(
    pending: { readonly text: string },
    error: string,
    retryable: boolean,
  ): void {
    console.error(`[faktor-chat] task start failed; the draft was kept: ${error}`);
    this.post({ type: 'startResult', goal: pending.text, ok: false, retryable });
  }

  /**
   * One durable board post was acknowledged (the host read it back at
   * `revision`); the panel may clear the submitted draft.
   */
  postBoardPosted(revision: number, token: string | null = null): void {
    this.post({ type: 'boardPosted', revision, token });
  }

  /**
   * The host answered a board post with a refusal: the panel releases its
   * in-flight lock but keeps the draft (the refusal reason is also posted as
   * a notice), correlated by the submission token.
   */
  postBoardRefused(token: string | null, reason: string): void {
    this.post({ type: 'boardRefused', token, reason });
  }

  /**
   * The result of ONE approval-card reply, correlated by the request id:
   * the panel releases that request's in-flight lock and, on a refusal,
   * keeps the request visible with the exact reason. `ok: true` means the
   * daemon accepted the reply (the pending snapshot drops the row); the
   * panel never retries implicitly.
   */
  postPermissionResult(permissionId: string, ok: boolean, reason: string | null = null): void {
    this.post({ type: 'permissionResult', permissionId, ok, reason });
  }

  /** One transient notice line (last error, control ack, ...). */
  postNotice(level: 'info' | 'error', message: string): void {
    this.post({ type: 'notice', level, message });
  }

  /**
   * ONE structured user-facing error. Raw backend text and typed codes ride
   * inside `technical` (the panel demotes them behind "Technical details");
   * the headline is human copy plus a next-step action.
   */
  postUserError(error: UserErrorProjection): void {
    this.post({ type: 'userError', error });
  }

  /** The bounded metadata list of the host-side composer attachments. */
  postAttachments(items: readonly unknown[]): void {
    this.post({ type: 'attachments', items });
  }

  /** The visible attachment set is gone (durable start, session switch). */
  postAttachmentsCleared(): void {
    this.post({ type: 'attachmentsCleared' });
  }

  /**
   * The stable blocked-stream reason (`null` = the stream left the blocked
   * state) and the raw resume position. The recovery affordances live in the
   * panel; the reason travels on its own message, and the raw cursor is
   * rendered only inside Diagnostics.
   */
  postStreamBlocked(reason: string | null, cursor: number | null = null): void {
    this.post({ type: 'streamBlocked', reason, cursor });
  }

  /**
   * T27/T28 presentation input: an explicit reconnect is in flight
   * (`recovering: true`), so the panel shows the T27 row until the stream
   * opens (or blocks again). The cursor-preservation semantics are unchanged.
   */
  postStreamRecovery(recovering: boolean): void {
    this.post({ type: 'streamRecovery', recovering: recovering === true });
  }

  private post(message: unknown): void {
    if (
      message !== null &&
      typeof message === 'object' &&
      (message as { type?: unknown }).type !== 'snapshot'
    ) {
      this.flushPendingSnapshot();
    }
    void this.view?.webview.postMessage(message);
  }

  private render(webview: vscode.Webview): string {
    const nonce = randomBytes(16).toString('hex');
    const scriptUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this.extensionUri, 'media', 'chat.js'),
    );
    const composerUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this.extensionUri, 'media', 'composer-state.js'),
    );
    const boardUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this.extensionUri, 'media', 'board-state.js'),
    );
    const historyUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this.extensionUri, 'media', 'history-state.js'),
    );
    const styleUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this.extensionUri, 'media', 'chat.css'),
    );
    const settingsStyleUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this.extensionUri, 'media', 'edition8.css'),
    );
    const settingsScriptUri = webview.asWebviewUri(
      vscode.Uri.joinPath(this.extensionUri, 'media', 'edition8.js'),
    );
    const settingsMarkup = renderEdition8AppMarkup({
      state: { snapshot: null, loading: false, error: null, focusControl: null },
      initialScreen: this.initialScreen,
      host: 'vscode',
      mode: 'settings',
      omitEmptyBanner: true,
    });
    const csp = [
      "default-src 'none'",
      `style-src ${webview.cspSource} 'unsafe-inline'`,
      `font-src ${webview.cspSource}`,
      `img-src ${webview.cspSource} data:`,
      `script-src 'nonce-${nonce}'`,
      "connect-src 'none'",
      "base-uri 'none'",
      "form-action 'none'",
    ].join('; ');
    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<link href="${settingsStyleUri}?v=${assetVersion()}" rel="stylesheet">
<link href="${styleUri}?v=${assetVersion()}" rel="stylesheet">
<title>Faktor</title>
</head>
<body data-surface="sidebar">
<div id="app">
  <header>
    <button id="btn-back-chat" class="toggle back-to-chat" type="button" title="Back to chat" aria-label="Back to chat" hidden>← Chat</button>
    <span id="brand">Faktor</span>
    <span id="ui-version" class="header-status" title="Installed Faktor build">v${assetVersion()}</span>
    <span id="daemon-dot" class="dot dot-stopped" aria-hidden="true"></span>
    <span id="daemon-text" class="header-status">Ready</span>
    <span class="spacer"></span>
    <button id="btn-overflow" class="toggle overflow-toggle" type="button" aria-expanded="false" aria-haspopup="menu" aria-controls="overflow-menu" title="Faktor menu" aria-label="Faktor menu">⋯</button>
    <button id="btn-control-center" class="toggle control-center-toggle" type="button" title="Open Faktor Settings (providers, context, tools, permissions, billing, appearance)" aria-label="Open Faktor Settings"><span class="control-center-label">Settings</span><span class="control-center-glyph" aria-hidden="true">⚙</span></button>
    <nav id="overflow-menu" class="overflow-menu" role="menu" aria-label="Faktor menu" hidden>
    <button id="menu-chat" class="menu-item" tabindex="-1" type="button" role="menuitem">Chat</button>
    <button id="menu-run-action" class="menu-item" tabindex="-1" type="button" role="menuitem" hidden></button>
    <button id="menu-inspect" class="menu-item" tabindex="-1" type="button" role="menuitem">Inspect</button>
    <button id="menu-history" class="menu-item" tabindex="-1" type="button" role="menuitem">History</button>
    <button id="menu-settings" class="menu-item" tabindex="-1" type="button" role="menuitem">Settings</button>
    <button id="menu-diagnostics" class="menu-item" tabindex="-1" type="button" role="menuitem">Diagnostics</button>
      <button id="menu-stop" class="menu-item destructive" tabindex="-1" type="button" role="menuitem" hidden>Stop current run</button>
    </nav>
  </header>
  <section id="diagnostics" class="view-panel diagnostics-view" role="region" aria-labelledby="diagnostics-title" tabindex="-1" hidden>
    <h2 id="diagnostics-title">Diagnostics</h2>
    <section id="service-diagnostics" class="service-diagnostics" aria-labelledby="service-diagnostics-title">
      <h3 id="service-diagnostics-title">Service</h3>
      <section id="meta">
        <div class="meta-row"><span class="meta-key">State</span><span id="machine-label">service stopped</span></div>
        <div class="meta-row"><span class="meta-key">Executable</span><span id="svc-executable">—</span></div>
        <div class="meta-row"><span class="meta-key">Trust</span><span id="trust-label">—</span></div>
        <div class="meta-row"><span class="meta-key">Readiness</span><span id="readiness-label">not requested</span></div>
        <div class="meta-row"><span class="meta-key">Address</span><span id="daemon-url">—</span></div>
        <div class="meta-row"><span class="meta-key">Port</span><span id="svc-port">—</span></div>
        <div class="meta-row"><span class="meta-key">Process</span><span id="svc-process">—</span></div>
        <div class="meta-row"><span class="meta-key">Restarts</span><span id="svc-restarts">0 / 0</span></div>
        <div class="meta-row"><span class="meta-key">Last failure</span><span id="svc-last-failure">—</span></div>
      </section>
      <section id="diagnostics-preflight" class="preflight diag-preflight" hidden aria-label="Preflight state">
        <div class="preflight-head">
          <span id="diag-preflight-dot" class="dot dot-stopped" aria-hidden="true"></span>
          <span id="diag-preflight-label" class="preflight-label">—</span>
        </div>
        <p id="diag-preflight-copy" class="preflight-copy muted"></p>
        <div id="diag-preflight-detail" class="muted preflight-detail" hidden></div>
        <div id="diag-preflight-actions" class="composer-actions preflight-actions"></div>
      </section>
      <div class="composer-actions">
        <button id="btn-start" class="secondary" type="button" title="Start the Faktor service explicitly">Start service</button>
        <button id="btn-stop" class="secondary" type="button" title="Stop the Faktor service (auto-start stays off until Start)">Stop service</button>
      </div>
    </section>
    <section id="session-diagnostics" aria-labelledby="session-diagnostics-title">
      <h3 id="session-diagnostics-title">Session</h3>
      <div class="meta-row"><span class="meta-key">Session</span><span id="session-title">none</span></div>
      <div class="meta-row"><span class="meta-key">Live updates</span><span id="stream-status">stopped</span></div>
      <div class="meta-row"><span class="meta-key">Index coverage</span><span id="index-coverage">not reported</span></div>
      <div class="composer-actions">
        <button id="btn-refresh" class="secondary" type="button" title="Refresh state">Refresh</button>
      </div>
    </section>
    <section id="run-diagnostics" aria-labelledby="run-diagnostics-title">
      <h3 id="run-diagnostics-title">Run</h3>
      <div class="meta-row"><span class="meta-key">Machine state</span><span id="diag-machine-state">unknown</span></div>
      <div class="meta-row"><span class="meta-key">Run state</span><span id="diag-run-state">—</span></div>
      <div class="meta-row"><span class="meta-key">Proof state</span><span id="diag-proof-state">—</span></div>
      <div class="meta-row"><span class="meta-key">Stream state</span><span id="diag-stream-state">stopped</span></div>
      <div class="meta-row"><span class="meta-key">Service state</span><span id="diag-daemon-state">stopped</span></div>
      <div class="meta-row"><span class="meta-key">Last error</span><span id="diag-last-error">—</span></div>
    </section>
    <div id="stream-cursor" class="muted" hidden></div>
    <section id="sandbox-setup" class="setup-card" aria-labelledby="sandbox-setup-title">
      <h3 id="sandbox-setup-title">Shell execution needs a decision</h3>
      <p id="sandbox-setup-copy" class="muted">The secure default remains fail-closed on this platform.</p>
      <button id="btn-open-sandbox-guidance" type="button">Open setup guidance</button>
      <div id="sandbox-guidance" class="sandbox-guidance" hidden>
        <p id="sandbox-guidance-text" class="muted">Run this on the machine that hosts the Faktor service. The service records the choice and applies it to agent shell and verification children; this panel cannot change shell execution policy.</p>
        <div class="setup-command">
          <code id="sandbox-command">faktor sandbox choose</code>
          <button id="btn-copy-sandbox-command" class="secondary" type="button" aria-label="Copy command faktor sandbox choose">Copy command</button>
        </div>
        <div id="sandbox-copy-status" class="muted" role="status" aria-live="polite"></div>
      </div>
    </section>
  </section>
  <section id="notices" class="sr-only" aria-live="polite" aria-atomic="false"></section>
  <main id="work-panel" class="view-panel work-panel" role="region" aria-label="Chat" tabindex="-1">
  <section id="attention-card" class="attention-card" hidden role="status" aria-live="polite" aria-labelledby="attention-card-title" data-problem-surface="attention">
    <div class="attention-head">
      <span id="attention-card-icon" class="attention-icon" aria-hidden="true"></span>
      <h2 id="attention-card-title">Needs attention</h2>
    </div>
    <p id="attention-card-cause" class="attention-cause muted"></p>
    <div id="attention-card-actions" class="attention-actions"></div>
    <details id="attention-technical" class="technical-disclosure" hidden>
      <summary>Technical details</summary>
      <pre id="attention-technical-body" class="excerpt"></pre>
    </details>
  </section>
  <section id="permission-card" class="card permission-card" hidden aria-labelledby="permission-card-title">
    <h2 id="permission-card-title">Faktor needs your approval</h2>
    <p id="permission-supporting" class="muted">Review the action, target and stated reason.</p>
    <div id="permission-announce" class="sr-only" role="status" aria-live="polite" aria-atomic="true"></div>
    <ul id="permission-list" class="permission-list" aria-label="Pending permission requests"></ul>
  </section>
  <section id="transcript-card" class="card transcript-card">
    <p id="welcome" class="welcome-line muted">Ask Faktor to start a conversation.</p>
    <div id="entries" role="log" aria-live="polite" aria-relevant="additions" aria-label="Conversation transcript" tabindex="-1"></div>
  </section>
  <form id="composer" data-intent="ask">
    <textarea id="goal" rows="1" aria-label="Ask Faktor" placeholder="Ask anything…"></textarea>
    <div class="composer-toolbar">
      <button id="btn-attach" class="composer-icon" type="button" title="Attach files" aria-label="Attach files">
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true" focusable="false"><path d="M10.5 4.5 5.6 9.4a1.7 1.7 0 0 0 2.4 2.4l5.2-5.2a3.2 3.2 0 0 0-4.5-4.5L3.2 7.6a4.7 4.7 0 0 0 6.6 6.6l4.1-4.1" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/></svg>
      </button>
      <button id="btn-clear-attachments" class="composer-icon" type="button" title="Clear attachments" aria-label="Clear attachments" hidden>
        <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true" focusable="false"><path d="m4 4 8 8M12 4l-8 8" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/></svg>
      </button>
      <div id="composer-intent" class="composer-intent" role="radiogroup" aria-label="Intent">
        <button id="btn-intent-ask" class="intent-option" type="button" role="radio" aria-checked="true" data-intent="ask" title="Ask a question without starting a task">Ask</button>
        <button id="btn-intent-task" class="intent-option" type="button" role="radio" aria-checked="false" data-intent="task" title="Start a task run with the options below">Task</button>
      </div>
      <span id="composer-model" class="chip chip-model" hidden></span>
      <span class="composer-spacer" aria-hidden="true"></span>
      <button id="btn-send" class="primary-action" type="submit">Send</button>
    </div>
    <ul id="attachment-list" aria-label="Attached files"></ul>
    <div id="attachment-notice" class="muted" hidden role="status"></div>
    <details id="contract-options" class="contract-options task-options">
      <summary>Task options</summary>
      <p class="muted">These apply only in Task mode. Ask sends a plain message and never attaches them.</p>
      <fieldset id="completion-contract" class="completion-contract">
        <legend>Finish actions for this task run</legend>
        <label><input type="radio" name="finish-level" id="contract-local" value="local" checked /> Leave changes locally</label>
        <label><input type="radio" name="finish-level" id="contract-commit" value="commit" /> Commit changes <span class="finish-implied muted">when verification passes</span></label>
        <label><input type="radio" name="finish-level" id="contract-push" value="push" /> Push branch <span class="finish-implied muted">also commits when verification passes</span></label>
        <label><input type="radio" name="finish-level" id="contract-pr" value="pr" /> Open pull request <span class="finish-implied muted">also commits and pushes</span></label>
      </fieldset>
      <label for="contract-constraints" class="composer-label">Constraints — conditions that cannot be violated</label>
      <textarea id="contract-constraints" class="contract-rows" rows="2" spellcheck="false" placeholder="One constraint per line"></textarea>
      <label for="contract-invariants" class="composer-label">Invariants — properties that must remain true</label>
      <textarea id="contract-invariants" class="contract-rows" rows="2" spellcheck="false" placeholder="One invariant per line"></textarea>
      <label for="contract-risks" class="composer-label">Risks — likely ways the change could fail</label>
      <textarea id="contract-risks" class="contract-rows" rows="2" spellcheck="false" placeholder="One risk per line"></textarea>
      <label for="contract-obligations" class="composer-label">Proof obligations — claims verification must evaluate</label>
      <textarea id="contract-obligations" class="contract-rows" rows="2" spellcheck="false" placeholder="One claim per line"></textarea>
      <p class="muted">Optional budget for this task run only. Empty uses the Faktor setting; an invalid value keeps the draft.</p>
      <label for="contract-budget-tokens" class="composer-label">Per-run token budget — non-negative integer, empty uses the setting</label>
      <input id="contract-budget-tokens" class="contract-budget" type="text" inputmode="numeric" autocomplete="off" spellcheck="false" placeholder="e.g. 200000" />
      <label for="contract-budget-cost" class="composer-label">Per-run cost budget — dollars, empty uses the setting</label>
      <input id="contract-budget-cost" class="contract-budget" type="text" inputmode="decimal" autocomplete="off" spellcheck="false" placeholder="e.g. $5.00" />
    </details>
    <div id="composer-state" class="state-strip composer-state" role="status" hidden>
      <span id="composer-state-glyph" class="state-strip-glyph" aria-hidden="true"></span>
      <span id="composer-state-label" class="state-strip-label"></span>
      <span id="composer-state-copy" class="state-strip-copy muted"></span>
    </div>
  </form>
  </main>
  <section id="inspect-panel" class="view-panel" role="region" aria-label="Inspect" tabindex="-1" hidden>
    <div class="inspect-nav">
      <div id="inspect-tabs" class="tablist" role="tablist" aria-label="Inspect subviews">
        <button id="subtab-overview" class="tab" type="button" role="tab" aria-selected="true" tabindex="0" aria-controls="inspect-subpanel-overview">Overview</button>
        <button id="subtab-plan" class="tab" type="button" role="tab" aria-selected="false" tabindex="-1" aria-controls="inspect-subpanel-plan">Plan</button>
        <button id="subtab-changes" class="tab" type="button" role="tab" aria-selected="false" tabindex="-1" aria-controls="inspect-subpanel-changes">Changes</button>
        <button id="subtab-verification" class="tab" type="button" role="tab" aria-selected="false" tabindex="-1" aria-controls="inspect-subpanel-verification">Verification</button>
        <button id="subtab-agents" class="tab" type="button" role="tab" aria-selected="false" tabindex="-1" aria-controls="inspect-subpanel-agents">Agents</button>
      </div>
      <div id="inspect-breadcrumb" class="inspect-breadcrumb" aria-label="Current scope">
        <span id="inspect-breadcrumb-path" class="inspect-breadcrumb-path">Inspect / Overview</span>
        <span id="inspect-breadcrumb-scope" class="muted"></span>
        <details id="inspect-breadcrumb-diagnostics" class="technical-disclosure breadcrumb-diagnostics">
          <summary>Advanced diagnostics</summary>
          <div id="inspect-breadcrumb-diagnostics-body" class="muted"></div>
        </details>
      </div>
    </div>
    <div id="inspect-subpanel-overview" class="inspect-subpanel" role="tabpanel" aria-labelledby="subtab-overview">
      <section id="overview-summary" class="card overview-summary" aria-labelledby="overview-summary-title">
        <h2 id="overview-summary-title">Overview</h2>
        <div id="overview-status" class="overview-status state-strip" role="status" hidden>
          <span id="overview-status-label" class="state-strip-label"></span>
          <span id="overview-status-copy" class="state-strip-copy muted"></span>
        </div>
        <dl id="overview-summary-list" class="overview-summary-list"></dl>
      </section>
      <section id="inspect-card" class="card">
        <div id="task-completion" class="completion"></div>
        <section id="run-details" class="run-details" aria-label="Run details">
          <h2>Run details</h2>
          <div class="run-strip">
            <span id="task-progress" class="run-meta" hidden></span>
            <span id="task-tests" class="run-meta" hidden></span>
            <span id="task-agents" class="run-meta" hidden></span>
          </div>
          <div id="run-state" class="state-strip run-state" role="status" hidden>
            <span id="run-state-glyph" class="state-strip-glyph" aria-hidden="true"></span>
            <span id="run-state-label" class="state-strip-label"></span>
            <span id="run-state-copy" class="state-strip-copy muted"></span>
            <span id="run-state-action" hidden></span>
          </div>
          <div id="task-goal" class="run-goal"></div>
        </section>
        <div id="cockpit" class="cockpit"></div>
      </section>
    </div>
    <div id="inspect-subpanel-plan" class="inspect-subpanel" role="tabpanel" aria-labelledby="subtab-plan" hidden></div>
    <div id="inspect-subpanel-changes" class="inspect-subpanel" role="tabpanel" aria-labelledby="subtab-changes" hidden>
      <section id="changes-card" class="card">
        <h2>Changes</h2>
        <div id="changes-toolbar" class="changes-toolbar">
          <div id="change-candidate" class="change-candidate"></div>
          <div class="changes-nav">
            <button id="btn-change-prev" class="secondary" type="button" title="Select the previous changed path">Previous change</button>
            <button id="btn-change-next" class="secondary" type="button" title="Select the next changed path">Next change</button>
            <button id="btn-change-review" type="button" title="Open the served immutable basis of the selected change (read-only)">Review change</button>
            <button id="btn-change-claim" class="secondary" type="button" title="Select the related criterion in Verification">Open claim</button>
            <button id="btn-change-return" class="secondary" type="button" title="Return to the current run in Work">Return to run</button>
            <button id="btn-change-recheck" class="secondary" type="button" title="Re-run the proof and projection reads and re-derive the current basis">Recheck current basis</button>
          </div>
          <div id="change-basis" class="change-basis"></div>
          <details id="change-advanced" class="technical-disclosure">
            <summary>Advanced details</summary>
            <div id="change-advanced-body" class="muted"></div>
          </details>
        </div>
        <div id="change-review-state" class="change-review-state" hidden role="status"></div>
        <ul id="changes-list" class="changes-list" aria-label="Changed paths served for this run"></ul>
        <div id="changes-empty" class="muted"></div>
        <section id="changes-unavailable" class="changes-unavailable" hidden>
          <h3 id="changes-unavailable-label" class="changes-unavailable-label" role="alert">Changes unavailable</h3>
          <p id="changes-unavailable-copy" class="muted"></p>
          <button id="btn-changes-recheck-unavailable" class="secondary" type="button">Recheck current basis</button>
        </section>
      </section>
    </div>
    <div id="inspect-subpanel-verification" class="inspect-subpanel" role="tabpanel" aria-labelledby="subtab-verification" hidden></div>
    <div id="inspect-subpanel-agents" class="inspect-subpanel" role="tabpanel" aria-labelledby="subtab-agents" hidden>
      <section id="agents-card" class="card" hidden>
        <h2>Agents</h2>
        <ul id="agent-list"></ul>
      </section>
      <details id="board-card" class="coordination-drawer" hidden>
        <summary>Coordination board</summary>
        <div id="board-header" class="muted">board: not read yet</div>
        <ul id="board-posts" class="board-posts" aria-label="Coordination board posts"></ul>
        <form id="board-post" class="board-composer">
          <label for="board-subject" class="composer-label">Subject</label>
          <input id="board-subject" type="text" maxlength="512" autocomplete="off" />
          <label for="board-body" class="composer-label">Body</label>
          <textarea id="board-body" rows="2" maxlength="16384"></textarea>
          <div id="board-draft-notice" class="muted" role="status" hidden></div>
          <div class="composer-actions">
            <button id="btn-board-read" type="button">Read board</button>
            <button id="btn-board-post" type="submit">Post</button>
          </div>
        </form>
      </details>
    </div>
  </section>
  <section id="history-panel" class="view-panel" role="region" aria-label="History" tabindex="-1" hidden>
    <div id="history-tabs" class="tablist history-tabs" role="tablist" aria-label="History surfaces">
      <button id="history-tab-conversations" class="tab" type="button" role="tab" aria-selected="true" tabindex="0" aria-controls="history-conversations">Conversations</button>
      <button id="history-tab-outcomes" class="tab" type="button" role="tab" aria-selected="false" tabindex="-1" aria-controls="history-outcomes">Outcomes</button>
      <button id="history-tab-account" class="tab" type="button" role="tab" aria-selected="false" tabindex="-1" aria-controls="history-account">Account</button>
    </div>
    <section id="history-conversations" role="tabpanel" aria-labelledby="history-tab-conversations">
    <section id="history-card" class="card">
      <h2>History</h2>
      <div id="history-controls" class="history-controls">
        <input id="history-search" type="search" placeholder="Search conversation titles" aria-label="Search conversations by title" autocomplete="off" maxlength="200" spellcheck="false" />
        <button id="btn-history-refresh" class="secondary" type="button" title="Re-read the durable conversation list">Refresh</button>
      </div>
      <div id="history-status" class="muted" role="status"></div>
      <ul id="history-list" class="history-list" aria-label="Durable conversations"></ul>
      <div id="history-note" class="muted"></div>
      <section id="history-unavailable" class="history-unavailable" hidden>
        <h3 id="history-unavailable-label" class="history-unavailable-label" role="alert">History unavailable</h3>
        <p id="history-unavailable-copy" class="muted">The conversation list could not be read.</p>
        <button id="btn-history-refresh-unavailable" type="button">Refresh history</button>
      </section>
    </section>
    </section>
    <section id="history-account" role="tabpanel" aria-labelledby="history-tab-account" hidden>
      <section id="account-card" class="card">
        <h2>Account &amp; usage</h2>
        <p id="account-copy" class="muted">Account not connected.</p>
        <div class="composer-actions">
          <button id="btn-account-read" class="secondary" type="button" title="Read the optional account and usage surface">Check account</button>
          <button id="btn-account-settings" class="secondary" type="button" title="Open the optional control-plane account settings">Account settings…</button>
        </div>
        <dl id="account-lines" class="overview-summary-list"></dl>
        <details class="technical-disclosure">
          <summary>Advanced diagnostics</summary>
          <div id="account-advanced" class="muted"></div>
        </details>
      </section>
    </section>
    <section id="history-outcomes" role="tabpanel" aria-labelledby="history-tab-outcomes" hidden>
      <section id="outcomes-card" class="card">
        <h2>Outcomes</h2>
        <div class="outcomes-controls">
          <button id="btn-outcomes-refresh" class="secondary" type="button" title="Re-read the ledger metrics and the newest run page">Refresh</button>
        </div>
        <div id="outcomes-status" class="muted" role="status"></div>
        <div id="outcomes-entitlements" class="muted outcomes-entitlements" role="status" hidden></div>
        <div id="outcomes-truncated" class="outcomes-truncated" role="note" hidden>Truncated sample</div>
        <div id="outcomes-metrics" class="outcomes-metrics"></div>
        <div id="outcomes-runs" class="outcomes-runs"></div>
        <button id="btn-outcomes-older" class="secondary" type="button" hidden>Older runs</button>
        <section id="outcomes-unavailable" class="outcomes-unavailable" hidden>
          <h3 id="outcomes-unavailable-label" class="outcomes-unavailable-label" role="alert">Cost unavailable</h3>
          <p id="outcomes-unavailable-copy" class="muted">The current authority has not supplied a cost value.</p>
          <details class="technical-disclosure">
            <summary>View details</summary>
            <div id="outcomes-unavailable-detail" class="muted"></div>
          </details>
        </section>
      </section>
    </section>
  </section>
  <section id="settings-panel" class="view-panel settings-view" role="region" aria-label="Settings" tabindex="-1" hidden>
<div id="settings-nav" class="settings-nav">
  <button id="settings-back-chat" class="back-to-chat" type="button" title="Back to chat" aria-label="Back to chat">← Chat</button>
  <span class="settings-nav-title">Settings</span>
</div>
${settingsMarkup}
  </section>
</div>
<script nonce="${nonce}">document.body.setAttribute('data-mode', 'chat');</script>
<script nonce="${nonce}" src="${settingsScriptUri}?v=${assetVersion()}"></script>
<script nonce="${nonce}" src="${boardUri}?v=${assetVersion()}"></script>
<script nonce="${nonce}" src="${historyUri}?v=${assetVersion()}"></script>
<script nonce="${nonce}" src="${composerUri}?v=${assetVersion()}"></script>
<script nonce="${nonce}" src="${scriptUri}?v=${assetVersion()}"></script>
</body>
</html>`;
  }
}
