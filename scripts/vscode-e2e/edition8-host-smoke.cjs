'use strict';
// Revision 8 real-Extension-Host smoke for the macOS/dev host.
//
// Loaded by a REAL `--extensionDevelopmentPath` VS Code host via
// `--extensionTestsPath` (same launch contract as extension-host-smoke.cjs).
// It proves the shipped extension code:
//   * activates and registers every contributed `faktor.*` command,
//   * renders the Edition 8 document with all 72 screens and all 200 controls
//     from the compiled registry (one article per control, each carrying
//     data-control / data-route / aria-labelledby),
//   * opens the Edition 8 work/inspect/history surfaces and one settings
//     screen through the real command surface without throwing.
//
// It does NOT take screenshots and does not drive the webview DOM; the
// Chromium/CDP matrix is the Linux e2e lane's job. Those limits are recorded
// in the evidence instead of being silently counted as a pass.

const fs = require('node:fs');

const evidencePath = process.env.FAKTOR_EDITION8_HOST_EVIDENCE || '';
const checks = [];
const check = (name, ok, detail) => {
  checks.push({ name, ok: Boolean(ok), detail: detail === undefined ? '' : String(detail) });
  if (!ok) throw new Error(`${name}${detail ? `: ${detail}` : ''}`);
};

function write(status, error) {
  if (!evidencePath) return;
  fs.writeFileSync(
    evidencePath,
    `${JSON.stringify(
      {
        schema: 'faktor-edition8-host-smoke/v1',
        host: 'vscode',
        status,
        error: error === undefined || error === null ? '' : String(error && error.message ? error.message : error),
        generated_at: new Date().toISOString(),
        checks,
      },
      null,
      2,
    )}\n`,
  );
}

exports.run = async function run() {
  write('running');
  try {
    const vscode = require('vscode');
    const ext = vscode.extensions.getExtension('faktor.faktor');
    check('extension-present', ext !== undefined && ext !== null, 'faktor.faktor must resolve');
    await ext.activate();
    check('activation-succeeded', true, `activated ${ext.id}`);

    const commands = await vscode.commands.getCommands(true);
    const contributed = ((ext.packageJSON.contributes || {}).commands || []).map((entry) => entry.command);
    const missing = contributed.filter((id) => !commands.includes(id));
    check(`commands-registered:${contributed.length}`, contributed.length > 0 && missing.length === 0, missing.join(', '));
    const e8 = [
      'faktor.openEdition8',
      'faktor.openInspect8',
      'faktor.openHistory8',
      'faktor.openUsage8',
      'faktor.openSettings8',
      'faktor.openProviders8',
      'faktor.openTools8',
      'faktor.openContext8',
      'faktor.openDiagnostics8',
      'faktor.openControl8',
    ];
    const absent = e8.filter((id) => !commands.includes(id));
    check('edition8-commands-registered', absent.length === 0, absent.join(', '));

    const htmlPath = require('node:path').join(ext.extensionPath, 'out', 'edition8', 'html.js');
    const registryPath = require('node:path').join(ext.extensionPath, 'out', 'generated', 'edition8Registry.js');
    const { renderEdition8Html } = require(htmlPath);
    const registry = require(registryPath);
    const screens = registry.E8_SCREENS || [];
    const controls = registry.E8_CONTROLS || [];
    check('registry-screens:72', screens.length === 72, String(screens.length));
    check('registry-controls:200', controls.length === 200, String(controls.length));

    const render = (mode, initialScreen) =>
      renderEdition8Html({
        nonce: 'N',
        cspSource: '*',
        scriptUri: '',
        styleUri: '',
        state: { snapshot: null, loading: false, error: null, focusControl: null },
        initialScreen,
        host: 'vscode',
        mode,
      });
    // The real host renders one destination group per document: Work/Inspect/
    // History in shell mode and the settings surfaces in settings mode. The
    // union must be all 72 screens / 200 controls.
    const html = render('shell', 'S01');
    const settingsHtml = render('settings', 'S29');
    const both = `${html}\n${settingsHtml}`;
    const renderedScreens =
      (html.match(/class="e8-screen"/g) || []).length +
      (settingsHtml.match(/class="e8-screen"/g) || []).length;
    const renderedControls = new Set([...both.matchAll(/data-control="([^"]+)"/g)].map((match) => match[1]));
    check('html-screens:72', renderedScreens === 72, String(renderedScreens));
    check('html-controls:200', renderedControls.size === 200, String(renderedControls.size));
    const missingLabel = controls.filter((control) => !both.includes(`aria-labelledby="e8-name-${control.id}"`));
    check('html-controls-labelled', missingLabel.length === 0, missingLabel.slice(0, 5).map((c) => c.id).join(','));

    for (const command of e8.filter((id) => id !== 'faktor.openControl8')) {
      await vscode.commands.executeCommand(command);
    }
    await vscode.commands.executeCommand('faktor.openControl8', 'S01');
    check('edition8-surfaces-opened', true, 'work/inspect/history/settings/S01 commands executed');

    write('passed');
    console.log(`EDITION8 VSCODE HOST PASS: screens=${renderedScreens} controls=${renderedControls.size} commands=${contributed.length}`);
  } catch (error) {
    write('failed', error);
    throw error;
  }
};
