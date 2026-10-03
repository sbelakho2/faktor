'use strict';
// Faktor VS Code Extension Host smoke -- this module is loaded by the PINNED
// VS Code extension host via `--extensionTestsPath` (scripts/vscode-e2e.sh).
//
// It runs against the built Faktor extension (`--extensionDevelopmentPath`
// = apps/vscode) and asserts the real extension-host contract:
//   * the `faktor.faktor` extension is present and activates,
//   * the contributed `faktor.*` commands are registered, and
//   * the compiled webview provider renders HTML carrying the strict CSP and
//     the composer/attachment surface elements.
//
// EXACT COVERAGE: this is a host-process smoke, not a pixel/screenshot test.
// It does not open a visible WebviewView or render the workbench UI; it
// exercises the extension's own compiled `render()` inside the real host.

const assert = require('node:assert');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checks = [];
const check = (name, condition, detail) => {
  const ok = Boolean(condition);
  checks.push({ name, ok, detail: detail === undefined ? '' : String(detail) });
  assert.ok(ok, `${name}${detail ? `: ${detail}` : ''}`);
};

const evidencePath = process.env.FAKTOR_VSCODE_E2E_EVIDENCE || '';

function writeEvidence(status, error) {
  if (!evidencePath) return;
  const record = {
    schema: 'faktor-vscode-e2e-evidence/v1',
    status,
    error: error ? String(error.stack || error) : '',
    extension: 'faktor.faktor',
    checks,
    wrote_at: new Date().toISOString(),
  };
  fs.writeFileSync(evidencePath, `${JSON.stringify(record, null, 2)}\n`);
}

exports.run = async function run() {
  try {
    const vscode = require('vscode');

    const ext = vscode.extensions.getExtension('faktor.faktor');
    check('extension-present', ext, 'faktor.faktor must resolve under --extensionDevelopmentPath');
    await ext.activate();
    check('activation-succeeded', true, `activated ${ext.id}`);

    const commands = await vscode.commands.getCommands(true);
    for (const command of [
      'faktor.openChat',
      'faktor.startServer',
      'faktor.stopServer',
      'faktor.refresh',
    ]) {
      check(`command-registered:${command}`, commands.includes(command), 'contributed command missing');
    }
    await vscode.commands.executeCommand('faktor.openChat');
    check('command-executed:faktor.openChat', true, 'the chat view command ran in the real host');

    // The compiled provider is part of the extension that is loaded in this
    // host; render its webview HTML with a webview-shaped stub. This proves
    // the actual surface the workbench would be handed (not a synthetic copy
    // of the source).
    const providerPath = path.join(ext.extensionPath, 'out', 'webview.js');
    check('compiled-provider-present', fs.existsSync(providerPath), providerPath);
    const providerModule = require(providerPath);
    check('webview-provider-exported', typeof providerModule.ChatViewProvider === 'function');
    check(
      'webview-view-type',
      providerModule.ChatViewProvider.viewType === 'faktor.chat',
      String(providerModule.ChatViewProvider.viewType),
    );

    const provider = new providerModule.ChatViewProvider(vscode.Uri.file(ext.extensionPath), {
      handle() {},
    });
    check('webview-render-callable', typeof provider.render === 'function');
    const webviewStub = {
      asWebviewUri: (uri) => uri,
      cspSource: 'vscode-webview://faktor-e2e',
    };
    const html = provider.render(webviewStub);
    check('webview-html-nonempty', typeof html === 'string' && html.length > 0);
    const markers = [
      '<title>Faktor</title>',
      'id="composer"',
      'id="goal"',
      'id="attachment-hint"',
      'id="attachment-list"',
      'id="attachment-notice"',
      'id="btn-attach"',
      'id="btn-clear-attachments"',
      "default-src 'none'",
      "script-src 'nonce-",
      "connect-src 'none'",
      'media/chat.js',
      'media/composer-state.js',
      'media/chat.css',
    ];
    for (const marker of markers) {
      check(`webview-html-has:${marker}`, html.includes(marker), 'marker missing from rendered HTML');
    }
    const htmlDigest = crypto.createHash('sha256').update(html, 'utf8').digest('hex');
    checks.push({ name: 'webview-html-sha256', ok: true, detail: htmlDigest });

    writeEvidence('passed', null);
  } catch (error) {
    writeEvidence('failed', error);
    throw error;
  }
};
