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
    });
    if (this.snapshot !== null) {
      this.post({ type: 'snapshot', snapshot: this.snapshot });
    }
    void this.host.onViewResolved?.();
  }

  /** Push a full state snapshot; the webview re-renders from it. */
  postSnapshot(snapshot: FaktorSnapshot): void {
    this.snapshot = snapshot;
    this.post({ type: 'snapshot', snapshot });
  }

  /** Deliver the decoded bytes of one expanded evidence artifact. */
  postEvidence(id: number, text: string, truncated: boolean): void {
    this.post({ type: 'evidence', id, text, truncated });
  }

  /**
   * The result of one `sendGoal`: the built-in composer clears its draft
   * ONLY when `ok` is true and the textarea still holds the submitted goal
   * (see media/composer-state.js).
   */
  postStartResult(goal: string, ok: boolean): void {
    this.post({ type: 'startResult', goal, ok });
  }

  /**
   * Restore one failed pending submission in the composer: the original
   * text travels back verbatim through `startResult` with `ok: false`, so
   * the draft (and its completion contract) is kept for the retry and never
   * silently lost. The failure message itself was already surfaced through
   * `postNotice`.
   */
  postSendMessageFailed(pending: { readonly text: string }, error: string): void {
    console.error(`[faktor-chat] task start failed; the draft was kept: ${error}`);
    this.post({ type: 'startResult', goal: pending.text, ok: false });
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

  /** One transient notice line (last error, control ack, ...). */
  postNotice(level: 'info' | 'error', message: string): void {
    this.post({ type: 'notice', level, message });
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
   * state). The recovery affordances live in the panel; the reason itself
   * is not part of the snapshot, so it travels on its own message.
   */
  postStreamBlocked(reason: string | null): void {
    this.post({ type: 'streamBlocked', reason });
  }

  focus(): void {
    void vscode.commands.executeCommand('faktor.chat.focus');
  }

  private post(message: unknown): void {
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
<div id="app" class="inspect-collapsed">
  <header>
    <span id="brand">Faktor</span>
    <span id="daemon-dot" class="dot dot-stopped" aria-hidden="true"></span>
    <span id="daemon-text">stopped</span>
    <span class="spacer"></span>
    <button id="btn-inspect" class="toggle" type="button" aria-expanded="false" title="Show run details, agents, board and evidence">Inspect</button>
    <button id="btn-refresh" class="secondary" type="button" title="Refresh state">Refresh</button>
    <button id="btn-stop" class="secondary" type="button" title="Stop the daemon">Stop</button>
    <button id="btn-start" class="secondary" type="button" title="Start the daemon">Start</button>
  </header>
  <section id="meta">
    <div class="meta-row"><span class="meta-key">Session</span><span id="session-title">none</span></div>
    <div class="meta-row"><span class="meta-key">State</span><span id="machine-label">daemon stopped</span></div>
    <div class="meta-row"><span class="meta-key">Stream</span><span id="stream-status">stopped</span></div>
  </section>
  <section id="stream-recovery" class="card" hidden>
    <h2>Event stream blocked</h2>
    <div id="stream-recovery-reason" class="warn" role="alert"></div>
    <div class="composer-actions">
      <button id="btn-refresh-snapshot" type="button" title="Re-read the durable state (the blocked cursor is not skipped)">Refresh from snapshot</button>
      <button id="btn-reconnect-stream" type="button" title="Reconnect from the last good cursor after the daemon is upgraded">Reconnect stream</button>
    </div>
    <div class="muted">The durable journal event that blocked the stream is never skipped. If the daemon predates this panel, upgrade it and reconnect; the daemon doctor (<code>faktor-cli doctor</code>) checks the journal.</div>
  </section>
  <section id="notices" aria-live="polite"></section>
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
    <div id="task-goal" class="run-goal"></div>
  </section>
  <section id="transcript-card" class="card transcript-card">
    <h2 id="transcript-title">Conversation</h2>
    <section id="welcome" class="welcome">
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
    <div id="entries" role="log" aria-live="polite" aria-relevant="additions" aria-label="Conversation transcript"></div>
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
        <legend>Task completion contract (Task mode; never sent by plain chat)</legend>
        <label><input type="checkbox" id="contract-commit" /> Commit when verified</label>
        <label><input type="checkbox" id="contract-push" /> Push</label>
        <label><input type="checkbox" id="contract-pr" /> Create PR</label>
      </fieldset>
    </details>
    <div class="composer-actions">
      <button id="btn-send" class="primary-action" type="submit">Run task</button>
      <button id="btn-new-task" class="secondary" type="button">New task…</button>
    </div>
  </form>
  <section id="inspect-card" class="card">
    <h2>Inspect</h2>
    <div id="task-completion" class="completion"></div>
    <div id="cockpit" class="cockpit"></div>
  </section>
  <section id="agents-card" class="card" hidden>
    <h2>Agents</h2>
    <ul id="agent-list"></ul>
  </section>
  <section id="board-card" class="card">
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
<script nonce="${nonce}" src="${boardUri}"></script>
<script nonce="${nonce}" src="${composerUri}"></script>
<script nonce="${nonce}" src="${scriptUri}"></script>
</body>
</html>`;
  }
}
