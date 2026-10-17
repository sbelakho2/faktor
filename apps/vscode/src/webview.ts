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

/** Messages the webview sends to the extension host. */
export interface ChatMessage {
  readonly type: string;
  readonly [key: string]: unknown;
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

export class ChatViewProvider implements vscode.WebviewViewProvider {
  public static readonly viewType = 'faktor.chat';

  private view: vscode.WebviewView | null = null;
  private snapshot: FaktorSnapshot | null = null;
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
    private readonly extensionUri: vscode.Uri,
    private readonly host: ChatHost,
  ) {}

  resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(this.extensionUri, 'media')],
    };
    view.webview.html = this.render(view.webview);
    view.webview.onDidReceiveMessage((message: ChatMessage) => {
      void this.host.handle(message);
    });
    view.onDidDispose(() => {
      this.view = null;
      if (this.flushTimer !== null) {
        clearTimeout(this.flushTimer);
        this.flushTimer = null;
      }
      this.pendingSnapshot = null;
    });
    if (this.flushTimer !== null) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    this.pendingSnapshot = null;
    if (this.snapshot !== null) {
      this.post({ type: 'snapshot', snapshot: this.snapshot });
    }
    void this.host.onViewResolved?.();
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

  focus(): void {
    void vscode.commands.executeCommand('faktor.chat.focus');
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
    const csp = [
      "default-src 'none'",
      `style-src ${webview.cspSource}`,
      `font-src ${webview.cspSource}`,
      `img-src ${webview.cspSource} data:`,
      `script-src 'nonce-${nonce}'`,
      "connect-src 'none'",
    ].join('; ');
    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<link href="${styleUri}" rel="stylesheet">
<title>Faktor</title>
</head>
<body>
<div id="app">
  <header>
    <span id="brand">Faktor</span>
    <span id="daemon-dot" class="dot dot-stopped" aria-hidden="true"></span>
    <span id="daemon-text" class="header-status">Ready</span>
    <span class="spacer"></span>
    <nav id="nav-switcher" class="nav-switcher" role="tablist" aria-label="Faktor views">
      <button id="nav-work" class="nav-tab" type="button" role="tab" aria-selected="true" tabindex="0" aria-controls="work-panel">Work</button>
      <button id="nav-inspect" class="nav-tab" type="button" role="tab" aria-selected="false" tabindex="-1" aria-controls="inspect-panel">Inspect</button>
      <button id="nav-history" class="nav-tab" type="button" role="tab" aria-selected="false" tabindex="-1" aria-controls="history-panel">History</button>
    </nav>
    <button id="btn-overflow" class="toggle overflow-toggle" type="button" aria-expanded="false" aria-haspopup="true" title="Diagnostics and service controls" aria-label="Diagnostics and service controls">⋯</button>
  </header>
  <section id="diagnostics" class="card diagnostics" hidden>
    <h2>Diagnostics</h2>
    <section id="meta">
      <div class="meta-row"><span class="meta-key">Session</span><span id="session-title">none</span></div>
      <div class="meta-row"><span class="meta-key">State</span><span id="machine-label">service stopped</span></div>
      <div class="meta-row"><span class="meta-key">Service</span><span id="daemon-url">—</span></div>
      <div class="meta-row"><span class="meta-key">Readiness</span><span id="readiness-label">not requested</span></div>
      <div class="meta-row"><span class="meta-key">Live updates</span><span id="stream-status">stopped</span></div>
      <div class="meta-row"><span class="meta-key">Index coverage</span><span id="index-coverage">not reported</span></div>
      <div class="meta-row"><span class="meta-key">Trust</span><span id="trust-label">—</span></div>
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
      <button id="btn-refresh" class="secondary" type="button" title="Refresh state">Refresh</button>
      <button id="btn-stop" class="secondary" type="button" title="Stop the Faktor service">Stop</button>
      <button id="btn-start" class="secondary" type="button" title="Start the Faktor service">Start</button>
    </div>
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
  <section id="stream-recovery" class="card" hidden aria-labelledby="stream-recovery-title">
    <h2 id="stream-recovery-title">Live updates paused</h2>
    <p id="stream-recovery-copy" class="recovery-copy">The client cannot read the next durable event. Nothing is skipped.</p>
    <div id="stream-recovery-reason" class="warn" role="alert"></div>
    <div class="composer-actions">
      <button id="btn-reconnect-stream" type="button" title="Reconnect from the last good position after the Faktor service is upgraded">Reconnect</button>
      <button id="btn-refresh-snapshot" class="secondary" type="button" title="Re-read the durable state (the blocked position is not skipped)">Refresh from snapshot</button>
      <button id="btn-wait-stream" class="secondary" type="button" title="Keep waiting; the last valid journal position is kept" hidden>Wait</button>
      <button id="btn-refresh-reconnecting" class="secondary" type="button" title="Re-read the durable state (the position is not skipped)" hidden>Refresh</button>
    </div>
    <details class="technical-disclosure">
      <summary>Technical details</summary>
      <div class="muted">The durable journal event that blocked the stream is never skipped. If the Faktor service predates this panel, upgrade it and reconnect; the service doctor (<code>faktor-cli doctor</code>) checks the journal.</div>
    </details>
  </section>
  <section id="notices" aria-live="polite"></section>
  <main id="work-panel" class="view-panel" role="tabpanel" aria-labelledby="nav-work">
  <section id="task-card" class="card run-card" hidden>
    <h2>Current run</h2>
    <div class="run-strip">
      <span id="task-state" class="state-chip" data-state="unknown">—</span>
      <span id="task-progress" class="run-meta" hidden></span>
      <span id="task-tests" class="run-meta" hidden></span>
      <span id="task-agents" class="run-meta" hidden></span>
      <span class="spacer"></span>
      <button id="btn-cancel-run" class="secondary" type="button">Cancel run</button>
    </div>
    <div id="run-state" class="state-strip run-state" role="status" hidden>
      <span id="run-state-glyph" class="state-strip-glyph" aria-hidden="true"></span>
      <span id="run-state-label" class="state-strip-label"></span>
      <span id="run-state-copy" class="state-strip-copy muted"></span>
    </div>
    <div id="task-goal" class="run-goal"></div>
  </section>
  <section id="permission-card" class="card permission-card" hidden aria-labelledby="permission-card-title">
    <h2 id="permission-card-title">Faktor needs your approval</h2>
    <p id="permission-supporting" class="muted">Review the action, target and stated reason.</p>
    <div id="permission-announce" class="sr-only" role="status" aria-live="polite" aria-atomic="true"></div>
    <ul id="permission-list" class="permission-list" aria-label="Pending permission requests"></ul>
  </section>
  <section id="transcript-card" class="card transcript-card">
    <h2 id="transcript-title">Conversation</h2>
    <section id="welcome" class="welcome">
      <section id="preflight-card" class="preflight" aria-labelledby="preflight-label" hidden>
        <div class="preflight-head">
          <span id="preflight-dot" class="dot dot-stopped" aria-hidden="true"></span>
          <h3 id="preflight-label" class="preflight-label">Faktor is stopped</h3>
        </div>
        <p id="preflight-copy" class="preflight-copy muted">Start the service when you are ready.</p>
        <div id="preflight-path-row" class="preflight-meta" hidden>
          <span class="preflight-key">Executable</span>
          <code id="preflight-path" class="preflight-path"></code>
        </div>
        <div id="preflight-trust-row" class="preflight-meta" hidden>
          <span class="preflight-key">Trust</span>
          <span id="preflight-trust" class="preflight-trust"></span>
        </div>
        <div id="preflight-detail" class="muted preflight-detail" hidden></div>
        <div id="preflight-actions" class="composer-actions preflight-actions"></div>
      </section>
      <div class="welcome-mark" aria-hidden="true">
        <svg viewBox="0 0 5 5" width="36" height="36" class="pixel pixel-done">
          <rect class="pixel-body" x="1" y="0" width="3" height="5"></rect>
          <rect class="pixel-body" x="0" y="1" width="5" height="3"></rect>
          <rect class="pixel-body" x="0" y="4" width="1" height="1"></rect>
          <rect class="pixel-body" x="4" y="4" width="1" height="1"></rect>
          <rect class="pixel-eye" x="1" y="1" width="1" height="1"></rect>
          <rect class="pixel-eye" x="3" y="1" width="1" height="1"></rect>
        </svg>
      </div>
      <p class="welcome-lead">Describe a task below. Faktor plans it, works in an isolated candidate workspace, and runs the checks.</p>
      <p class="welcome-lead">Follow the run as it works — every edit, test and verification result lands in the conversation.</p>
      <p class="welcome-lead">Changes only land after verification passes; you stay in control of the final step.</p>
      <p class="welcome-hint muted">Start with one of these:</p>
      <ul class="welcome-prompts" aria-label="Example prompts">
        <li><button type="button" class="welcome-chip" data-prompt="Find and fix the failing tests">Find and fix the failing tests</button></li>
        <li><button type="button" class="welcome-chip" data-prompt="Review this change for correctness and edge cases">Review this change for correctness and edge cases</button></li>
        <li><button type="button" class="welcome-chip" data-prompt="Implement the next item in the plan">Implement the next item in the plan</button></li>
      </ul>
    </section>
    <div id="entries" role="log" aria-live="polite" aria-relevant="additions" aria-label="Conversation transcript" tabindex="-1"></div>
  </section>
  <form id="composer">
    <label for="goal" class="composer-label">Ask Faktor…</label>
    <textarea id="goal" rows="3" placeholder="Describe what you want done."></textarea>
    <div class="composer-chips">
      <span id="composer-model" class="chip chip-model" hidden></span>
      <button id="btn-attach" class="chip chip-action" type="button" title="Attach files through the host file picker">Attach files…</button>
      <button id="btn-clear-attachments" class="chip chip-action" type="button" hidden>Clear all</button>
      <span id="attachment-hint" class="chip-hint muted">Drop files here or paste an image (Ctrl/Cmd+V).</span>
    </div>
    <ul id="attachment-list" aria-label="Attached files"></ul>
    <div id="attachment-notice" class="muted" hidden role="status"></div>
    <details id="finish-options" class="finish-options">
      <summary>Finish when done</summary>
      <fieldset id="completion-contract" class="completion-contract">
        <legend>Finish actions for this run (Run task only; plain chat never sends this)</legend>
        <label><input type="radio" name="finish-level" id="contract-local" value="local" checked /> Leave changes locally</label>
        <label><input type="radio" name="finish-level" id="contract-commit" value="commit" /> Commit changes <span class="finish-implied muted">when verification passes</span></label>
        <label><input type="radio" name="finish-level" id="contract-push" value="push" /> Push branch <span class="finish-implied muted">also commits when verification passes</span></label>
        <label><input type="radio" name="finish-level" id="contract-pr" value="pr" /> Open pull request <span class="finish-implied muted">also commits and pushes</span></label>
      </fieldset>
    </details>
    <details id="contract-options" class="contract-options">
      <summary>Acceptance &amp; contract</summary>
      <p class="muted">The run must respect these. One entry per line; at most 64 lines of 512 bytes each, sent with this task start.</p>
      <label for="contract-constraints" class="composer-label">Constraints — conditions that cannot be violated</label>
      <textarea id="contract-constraints" class="contract-rows" rows="2" spellcheck="false" placeholder="One constraint per line"></textarea>
      <label for="contract-invariants" class="composer-label">Invariants — properties that must remain true</label>
      <textarea id="contract-invariants" class="contract-rows" rows="2" spellcheck="false" placeholder="One invariant per line"></textarea>
      <label for="contract-risks" class="composer-label">Risks — likely ways the change could fail</label>
      <textarea id="contract-risks" class="contract-rows" rows="2" spellcheck="false" placeholder="One risk per line"></textarea>
      <label for="contract-obligations" class="composer-label">Proof obligations — claims verification must evaluate</label>
      <textarea id="contract-obligations" class="contract-rows" rows="2" spellcheck="false" placeholder="One claim per line"></textarea>
      <p class="muted">Optional budget for THIS task start only. Empty uses the Faktor setting; an invalid value refuses the start and keeps the draft.</p>
      <label for="contract-budget-tokens" class="composer-label">Per-run token budget — non-negative integer, empty uses the setting</label>
      <input id="contract-budget-tokens" class="contract-budget" type="text" inputmode="numeric" autocomplete="off" spellcheck="false" placeholder="e.g. 200000" />
      <label for="contract-budget-cost" class="composer-label">Per-run cost budget — micro-USD, exact non-negative integer, empty uses the setting</label>
      <input id="contract-budget-cost" class="contract-budget" type="text" inputmode="numeric" autocomplete="off" spellcheck="false" placeholder="e.g. 5000000" />
    </details>
    <div id="composer-state" class="state-strip composer-state" role="status" hidden>
      <span id="composer-state-glyph" class="state-strip-glyph" aria-hidden="true"></span>
      <span id="composer-state-label" class="state-strip-label"></span>
      <span id="composer-state-copy" class="state-strip-copy muted"></span>
    </div>
    <div class="composer-actions">
      <button id="btn-send" class="primary-action" type="submit">Run task</button>
      <button id="btn-new-task" class="secondary" type="button">New task…</button>
    </div>
  </form>
  </main>
  <section id="inspect-panel" class="view-panel" role="tabpanel" aria-labelledby="nav-inspect" hidden>
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
      <section id="inspect-card" class="card">
        <div id="task-completion" class="completion"></div>
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
      <section id="board-card" class="card" hidden>
        <h2>Board</h2>
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
      </section>
    </div>
  </section>
  <section id="history-panel" class="view-panel" role="tabpanel" aria-labelledby="nav-history" hidden>
    <div id="history-tabs" class="tablist history-tabs" role="tablist" aria-label="History surfaces">
      <button id="history-tab-conversations" class="tab" type="button" role="tab" aria-selected="true" tabindex="0" aria-controls="history-conversations">Conversations</button>
      <button id="history-tab-outcomes" class="tab" type="button" role="tab" aria-selected="false" tabindex="-1" aria-controls="history-outcomes">Outcomes</button>
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
</div>
<script nonce="${nonce}" src="${boardUri}"></script>
<script nonce="${nonce}" src="${historyUri}"></script>
<script nonce="${nonce}" src="${composerUri}"></script>
<script nonce="${nonce}" src="${scriptUri}"></script>
</body>
</html>`;
  }
}
