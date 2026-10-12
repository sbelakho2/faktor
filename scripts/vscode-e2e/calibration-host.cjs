'use strict';
// Faktor VS Code Extension Host E2E -- bounded calibration control.
//
// Loaded by the SAME pinned VS Code extension host, through the SAME launch
// path and Xvfb facility as the real matrix cells, immediately before them
// (scripts/vscode-e2e.sh). It measures this runner's Electron cold-start +
// extension-activation cost so the lane can derive its per-cell timeout and
// the smoke module's wait factor from a same-run control instead of trusting
// the documented product-runner constants.
//
// It records when this module became live in the extension host and when the
// Faktor extension activated. It never asserts a verdict: the harness reads
// the timestamps and fails typed if the control cannot be measured. Marker
// lines go to FAKTOR_VSCODE_E2E_PROGRESS when the harness provides it.

const fs = require('node:fs');
const path = require('node:path');

function progressStage(stage, detail) {
  const progressPath = process.env.FAKTOR_VSCODE_E2E_PROGRESS || '';
  if (!progressPath) return;
  try {
    fs.appendFileSync(
      progressPath,
      `${new Date().toISOString()} stage=${stage}${detail ? ` ${String(detail).slice(0, 200)}` : ''}\n`,
    );
  } catch {
    /* markers are diagnostics; a failed marker write must not mask the measurement */
  }
}

exports.run = async function run() {
  progressStage('host-module-live', 'calibration');
  const record = {
    schema: 'faktor-vscode-e2e-calibration/v1',
    status: 'calibrated',
    host_module_live_at: Date.now(),
    activation_at: 0,
    activated: '',
    activation_error: '',
    pid: process.pid,
  };
  try {
    const vscode = require('vscode');
    const ext = vscode.extensions.getExtension('faktor.faktor');
    if (!ext) {
      record.activation_error = 'faktor.faktor not found under --extensionDevelopmentPath';
    } else {
      await ext.activate();
      record.activated = ext.id;
    }
  } catch (error) {
    record.activation_error = String((error && error.message) || error);
  }
  record.activation_at = Date.now();
  progressStage('extension-activated', `calibration activated=${record.activated || 'none'}`);
  const out = process.env.FAKTOR_VSCODE_E2E_EVIDENCE;
  if (!out) {
    throw new Error('FAKTOR_VSCODE_E2E_EVIDENCE is not set');
  }
  fs.mkdirSync(path.dirname(out), { recursive: true });
  fs.writeFileSync(out, `${JSON.stringify(record, null, 2)}\n`);
};
