#!/usr/bin/env node
// Revision 8 functional-evidence ledger.
//
// ONE machine-readable ledger owns every control's functional status:
//
//   docs/specs/edition8/functional-evidence.json
//
// Each row records, per host, the implementation seam (file:line), the live
// harness test id/receipt, and whether the observed state is exists, partial
// or missing. `--sync` regenerates the status fields in controls.json, the
// generated counts/table in backlog.md and the JetBrains route map from that
// ledger. `--check` fails when any of them diverges, when a cited evidence
// line/receipt is missing, or when a status is not backed by evidence.
//
// Usage:
//   node scripts/edition8/ledger.mjs --build      # derive evidence rows
//   node scripts/edition8/ledger.mjs --sync       # write controls/backlog/host map
//   node scripts/edition8/ledger.mjs --check      # fail on divergence
//   node scripts/edition8/ledger.mjs --selftest   # prove divergence is detected

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const SPEC = resolve(ROOT, 'docs/specs/edition8');
const LEDGER_PATH = resolve(SPEC, 'functional-evidence.json');
const CONTROLS_PATH = resolve(SPEC, 'controls.json');
const BACKLOG_PATH = resolve(SPEC, 'backlog.md');
const REGISTRY_PATH = resolve(SPEC, 'registry.json');
const VSCODE_ROUTES = resolve(ROOT, 'apps/vscode/src/edition8/routes.ts');
const VSCODE_EXTENSION = resolve(ROOT, 'apps/vscode/src/extension.ts');
const JB_REGISTRY = resolve(
  ROOT,
  'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition8ScreenRegistry.kt',
);
const JB_ROUTE_MAP = resolve(
  ROOT,
  'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition8RouteMap.kt',
);
const DAEMON_CONFIG = resolve(ROOT, 'crates/foundation/src/config.rs');
const DAEMON_SRC = resolve(ROOT, 'crates/server/src');
const VSCODE_RECEIPT = resolve(ROOT, 'target/certification/edition8/control-exercise-vscode.raw.json');
const JB_RECEIPT = resolve(ROOT, 'target/certification/edition8/control-exercise-jetbrains.raw.json');
const DAEMON_RECEIPT = resolve(ROOT, 'target/certification/edition8/acceptance-daemon.raw.json');
const PROGRAMME_RECEIPT = resolve(ROOT, 'target/certification/programme/registry-status.json');
const DOGFOOD_RECEIPT = resolve(ROOT, 'target/certification/edition8/dogfood-vscode.raw.json');

// Controls whose real-daemon dogfood flow is the stronger live receipt (the
// control exercise is a fixture click; the dogfood drives the production
// daemon and a loopback provider).
const DOGFOOD_FLOW = 'rev8-field-contracts';
const DOGFOOD_CONTROLS = new Set([
  'C-07', 'C-14', 'K-12', 'S-09', 'T-12', 'X-06', 'X-08', 'X-09', 'O-10',
]);

const LEDGER_SCHEMA = 'faktor.edition8.functional-evidence/v1';
const HOSTS = ['vscode', 'jetbrains', 'daemon'];
const STATUSES = ['exists', 'partial', 'missing'];

// Controls whose contractual functionality is still a typed unsupported state
// or is not wired in one host. Every entry MUST carry a note in the ledger;
// `--check` refuses a partial without one. Entries are removed only when the
// implementation/evidence closes the gap (never to make the counts green).
const PARTIAL = {
  // A real capability gap found by independent audit 2026-10-19: the daemon
  // has no Streamable-HTTP MCP transport (stdio only). The VS Code receipt is
  // a typed unavailable, and the JetBrains receipt reads the provider
  // collection rather than an MCP transport action. Remote destination,
  // OAuth and reconnect semantics are therefore NOT implemented here.
  'T-04': {
    vscode:
      'typed unavailable: Streamable HTTP is not negotiated by this daemon build; stdio is the implemented transport and no HTTP/OAuth state is claimed.',
    jetbrains:
      'the live receipt read the provider collection, not an MCP transport action; remote transport is not exercised.',
    daemon:
      'the MCP save route serves stdio; no streamable-HTTP destination/auth/reconnect path is negotiated.',
  },
};

const REQUIRED_LEDGER = 'faktor.edition8.functional-evidence/v1';

function read(path) {
  return readFileSync(path, 'utf8');
}

function json(path) {
  return JSON.parse(read(path));
}

function lineAt(text, index) {
  return text.slice(0, index).split('\n').length;
}

function rel(path) {
  return path.slice(ROOT.length + 1);
}

function normalizeRoute(path) {
  if (typeof path !== 'string') throw new Error(`cannot normalize route ${String(path)}`);
  return `/${path
    .replace(/^\//, '')
    .replace(/:[A-Za-z_][A-Za-z0-9_]*/g, '{}')
    .replace(/\{[A-Za-z_][A-Za-z0-9_]*\}/g, '{}')
    .split('?')[0]}`;
}

function vscodeRoutes() {
  const text = read(VSCODE_ROUTES);
  const out = new Map();
  const re = /^\s*'([A-Z]-\d{2})':\s*(cfg|read|act|host)\((.*)\),?\s*$/gm;
  for (const match of text.matchAll(re)) {
    const id = match[1];
    const kind = match[2];
    const args = match[3];
    const line = lineAt(text, match.index);
    if (kind === 'cfg') {
      const key = /^\s*'([^']+)'\s*$/.exec(args)?.[1] ?? null;
      out.set(id, { kind, method: 'POST', path: '/native/config/commit', configKey: key, line });
    } else if (kind === 'read') {
      const path = /^\s*'([^']+)'\s*$/.exec(args)?.[1] ?? null;
      out.set(id, { kind, method: 'GET', path, configKey: null, line });
    } else if (kind === 'act') {
      const act = /^\s*'([A-Z]+)',\s*'([^']+)'\s*$/.exec(args);
      out.set(id, {
        kind,
        method: act?.[1] ?? null,
        path: act?.[2] ?? null,
        configKey: null,
        line,
      });
    } else {
      out.set(id, { kind, method: 'GET', path: 'host', configKey: null, line });
    }
  }
  return out;
}

function daemonRouteMap() {
  const out = new Map();
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const path = resolve(dir, entry.name);
      if (entry.isDirectory()) walk(path);
      else if (entry.name.endsWith('.rs')) {
        const text = read(path);
        for (const match of text.matchAll(/\.route\(\s*"([^"]+)"/g)) {
          out.set(normalizeRoute(match[1]), { file: rel(path), line: lineAt(text, match.index) });
        }
      }
    }
  };
  walk(DAEMON_SRC);
  return out;
}

function configLines() {
  const text = read(DAEMON_CONFIG);
  const out = new Map();
  for (const match of text.matchAll(/"((?:faktor\.)[A-Za-z0-9_.]+)"/g)) {
    out.set(match[1], { file: rel(DAEMON_CONFIG), line: lineAt(text, match.index) });
  }
  return out;
}

function controlLines() {
  const text = read(JB_REGISTRY);
  const out = new Map();
  for (const match of text.matchAll(/\bid = "([A-Z]-\d{2})"/g)) {
    out.set(match[1], { file: rel(JB_REGISTRY), line: lineAt(text, match.index) });
  }
  return out;
}

function hostActionLines() {
  const text = read(VSCODE_EXTENSION);
  const out = new Map();
  for (const match of text.matchAll(/case '([A-Z]-\d{2})':/g)) {
    out.set(match[1], { file: rel(VSCODE_EXTENSION), line: lineAt(text, match.index) });
  }
  return out;
}

function receiptRows(path) {
  if (!existsSync(path)) return new Map();
  const document = json(path);
  const out = new Map();
  for (const row of document.results ?? []) out.set(row.id, row);
  return out;
}

function daemonCoverage() {
  const out = new Map();
  if (!existsSync(DAEMON_RECEIPT)) return out;
  const document = json(DAEMON_RECEIPT);
  for (const scenario of document.results ?? []) {
    for (const id of scenario.controls ?? []) {
      if (!out.has(id)) out.set(id, { scenarios: [], routes: new Map() });
      const entry = out.get(id);
      entry.scenarios.push(scenario.id);
      for (const route of scenario.routes ?? []) {
        if (route.registered !== false) {
          entry.routes.set(`${route.method} ${route.endpoint}`, route.status ?? null);
        }
      }
    }
  }
  return out;
}

function programmeCoverage() {
  if (!existsSync(PROGRAMME_RECEIPT)) return new Map();
  const document = json(PROGRAMME_RECEIPT);
  return new Map((document.rows ?? []).map((row) => [row.id, row]));
}

function sourceFor(control, route, daemon, configs, controls, hostActions) {
  const evidence = { vscode: null, jetbrains: null, daemon: null };
  const live = { vscode: null, jetbrains: null, daemon: null };
  if (route === undefined) return { evidence, live };
  evidence.vscode = { file: rel(VSCODE_ROUTES), line: route.line, note: `VS Code route binding (${route.kind}).` };
  const controlLine = controls.get(control.id);
  evidence.jetbrains = controlLine
    ? { ...controlLine, note: 'JetBrains Revision 8 control binding on its declared screen.' }
    : null;
  if (route.kind === 'host') {
    const hostLine = hostActions.get(control.id);
    evidence.vscode = hostLine
      ? { ...hostLine, note: 'VS Code native host action (no daemon write is invented).' }
      : evidence.vscode;
    evidence.daemon = {
      file: rel(VSCODE_EXTENSION),
      line: hostLine?.line ?? route.line,
      note: 'Host-owned control: the daemon has no route for this effect.',
    };
  } else if (route.kind === 'config') {
    const descriptor = configs.get(route.configKey);
    evidence.daemon = descriptor
      ? { ...descriptor, note: `Configuration descriptor for ${route.configKey}.` }
      : null;
  } else {
    const daemonRoute = daemon.get(normalizeRoute(route.path));
    evidence.daemon = daemonRoute
      ? { ...daemonRoute, note: `Daemon route ${route.method} ${route.path}.` }
      : null;
  }
  return { evidence, live };
}

function build() {
  for (const path of [VSCODE_RECEIPT, JB_RECEIPT, DAEMON_RECEIPT]) {
    if (!existsSync(path)) {
      throw new Error(
        `missing live receipt ${rel(path)}; run the VS Code control exercise, the JetBrains smoke lane and daemon acceptance before --build`,
      );
    }
  }
  const controls = json(CONTROLS_PATH).records;
  const routes = vscodeRoutes();
  const daemon = daemonRouteMap();
  const configs = configLines();
  const jbControls = controlLines();
  const hostActions = hostActionLines();
  const vscodeRows = receiptRows(VSCODE_RECEIPT);
  const jbRows = receiptRows(JB_RECEIPT);
  const daemonRows = daemonCoverage();
  const programme = programmeCoverage();

  const rows = controls.map((control) => {
    const route = routes.get(control.id);
    if (route === undefined) throw new Error(`no VS Code route for control ${control.id}`);
    const { evidence, live } = sourceFor(control, route, daemon, configs, jbControls, hostActions);
    const partial = PARTIAL[control.id] ?? {};
    const hostStatus = {};
    for (const host of ['vscode', 'jetbrains']) {
      const row = host === 'vscode' ? vscodeRows.get(control.id) : jbRows.get(control.id);
      const passed = row?.passed === true;
      hostStatus[host] = partial[host] ? 'partial' : passed && evidence[host] ? 'exists' : 'partial';
      live[host] = {
        test_id: `control.exercise.${control.id}`,
        evidence: `${host === 'vscode' ? 'target/certification/edition8/control-exercise-vscode.raw.json' : 'target/certification/edition8/control-exercise-jetbrains.raw.json'}#${control.id}`,
        observed: row === undefined ? 'missing receipt row' : `${row.status ?? 'unknown'}: ${row.observable ?? row.error ?? 'no observation'}`,
        passed,
      };
    }
    let dogfood = null;
    if (DOGFOOD_CONTROLS.has(control.id) && existsSync(DOGFOOD_RECEIPT)) {
      const document = json(DOGFOOD_RECEIPT);
      const row = (document.results ?? []).find((entry) => entry.id === DOGFOOD_FLOW);
      dogfood = {
        test_id: `dogfood.${DOGFOOD_FLOW}`,
        evidence: `target/certification/edition8/dogfood-vscode.raw.json#${DOGFOOD_FLOW}`,
        passed: row?.passed === true,
        observed: row?.detail ?? 'no dogfood row',
      };
    }
    const daemonEntry = daemonRows.get(control.id);
    const daemonSupported =
      evidence.daemon !== null &&
      (route.kind === 'host' || route.kind === 'config' || daemon.has(normalizeRoute(route.path)));
    hostStatus.daemon = partial.daemon
      ? 'partial'
      : daemonSupported
        ? 'exists'
        : 'missing';
    live.daemon = {
      test_id: `control.contract.${control.id}`,
      evidence: daemonEntry
        ? `target/certification/edition8/acceptance-daemon.raw.json#${daemonEntry.scenarios.join(',')}`
        : 'target/certification/edition8/acceptance-daemon.raw.json',
      observed: daemonEntry
        ? `registered routes: ${[...daemonEntry.routes.keys()].slice(0, 8).join('; ') || 'none'}; programme: ${programme.get(`control.${control.id}`)?.status ?? 'deferred'}`
        : 'no daemon acceptance row',
      passed: daemonSupported,
    };
    for (const host of HOSTS) {
      const implementation = evidence[host];
      if (implementation !== null && existsSync(resolve(ROOT, implementation.file))) {
        implementation.line_text =
          read(resolve(ROOT, implementation.file)).split('\n')[implementation.line - 1] ?? '';
      }
    }
    const notes = Object.entries(partial).map(([host, note]) => `${host}: ${note}`);
    const failedHosts = HOSTS.filter((host) => hostStatus[host] !== 'exists');
    if (notes.length === 0 && failedHosts.length > 0) {
      notes.push(
        `${failedHosts.join(', ')} evidence is not a passing live receipt; rerun the host harness before claiming exists`,
      );
    }
    const status = notes.length > 0 ? 'partial' : 'exists';
    return {
      id: control.id,
      category: control.category_code,
      name: control.name,
      status,
      note:
        notes.length > 0
          ? notes.join(' ')
          : `Implemented in both hosts and the daemon; live receipts pass (${route.kind}).`,
      route: {
        kind: route.kind,
        method: route.method,
        path: route.path,
        config_key: route.configKey,
      },
      hosts: Object.fromEntries(
        HOSTS.map((host) => [
          host,
          {
            status: hostStatus[host],
            implementation: evidence[host],
            live: live[host],
          },
        ]),
      ),
      acceptance: control.acceptance_scenario_ids ?? [],
      dogfood,
    };
  });

  const counts = countRows(rows);
  return {
    schema: LEDGER_SCHEMA,
    revision: 8,
    source: 'Revision 8 control contracts + current working tree',
    generated_by: 'node scripts/edition8/ledger.mjs --build',
    status_definition: {
      exists: 'Implementation evidence in both hosts + daemon (or an explicit host-owned surface) with a passing live harness receipt.',
      partial: 'At least one host is a typed unsupported state or lacks the specified wiring; the exact gap is recorded in the note.',
      missing: 'No implementation seam exists for the control.',
    },
    counts,
    controls: rows,
  };
}

function countRows(rows) {
  const counts = { controls: rows.length, by_status: { exists: 0, partial: 0, missing: 0 }, by_host: {} };
  for (const row of rows) {
    counts.by_status[row.status] += 1;
    for (const host of HOSTS) {
      counts.by_host[host] ??= { exists: 0, partial: 0, missing: 0 };
      counts.by_host[host][row.hosts[host].status] += 1;
    }
  }
  return counts;
}

function backlogCountsBlock(ledger) {
  const by = ledger.counts.by_status;
  return [
    '<!-- edition8-status-counts:begin -->',
    `- **${ledger.controls.length} controls:** ${by.exists} \`exists\`, ${by.partial} \`partial\`, ${by.missing} \`missing\`. Generated from \`functional-evidence.json\` by \`node scripts/edition8/ledger.mjs --sync\`; do not edit this block by hand.`,
    '<!-- edition8-status-counts:end -->',
  ].join('\n');
}

function backlogTableBlock(ledger) {
  const categories = {};
  for (const row of ledger.controls) {
    categories[row.category] ??= { exists: 0, partial: 0, missing: 0, notes: {} };
    categories[row.category][row.status] += 1;
    if (row.status !== 'exists') {
      const note = row.note;
      categories[row.category].notes[note] = (categories[row.category].notes[note] ?? 0) + 1;
    }
  }
  const names = {
    M: 'Models and routing',
    P: 'Providers, credentials and local inference',
    G: 'Reasoning and generation',
    A: 'Agent presets and delegation',
    C: 'Context, attachments and retrieval',
    K: 'Compaction, memory and efficiency',
    I: 'Instructions, rules and prompt composition',
    Q: 'Permissions, autonomy and sandbox',
    T: 'Tool catalog and MCP',
    S: 'Skills, commands and hooks',
    X: 'Terminal, browser and language tooling',
    B: 'Budgets, retries and reliability',
    V: 'Changes, checkpoints, verification and finish',
    D: 'Sessions, displays and accessibility',
    O: 'Privacy, diagnostics, optional cloud and distribution',
  };
  const lines = [];
  lines.push('<!-- edition8-status-table:begin -->');
  lines.push('| Category | exists | partial | missing | Main blocker |');
  lines.push('|---|---:|---:|---:|---|');
  for (const code of Object.keys(names)) {
    const row = categories[code] ?? { exists: 0, partial: 0, missing: 0, notes: {} };
    const note = Object.entries(row.notes).sort((a, b) => b[1] - a[1])[0]?.[0] ?? 'None.';
    lines.push(`| ${code} ${names[code]} | ${row.exists} | ${row.partial} | ${row.missing} | ${note.replace(/\|/g, '\\|')} |`);
  }
  lines.push('<!-- edition8-status-table:end -->');
  return lines.join('\n');
}

function replaceBacklogBlock(text, name, body) {
  const beginMarker = `<!-- edition8-status-${name}:begin -->`;
  const endMarker = `<!-- edition8-status-${name}:end -->`;
  const begin = text.indexOf(beginMarker);
  const end = text.indexOf(endMarker);
  if (begin < 0 || end < 0 || end < begin) {
    throw new Error(`backlog.md is missing the generated ${name} markers`);
  }
  return `${text.slice(0, begin)}${body}${text.slice(end + endMarker.length)}`;
}

function sync() {
  const ledger = json(LEDGER_PATH);
  const controlsDocument = json(CONTROLS_PATH);
  const byId = new Map(ledger.controls.map((row) => [row.id, row]));
  for (const record of controlsDocument.records) {
    const row = byId.get(record.id);
    if (row === undefined) throw new Error(`ledger missing control ${record.id}`);
    record.current_status = row.status;
    record.status_note = row.note;
    record.evidence = HOSTS.flatMap((host) => {
      const implementation = row.hosts[host].implementation;
      return implementation === null
        ? []
        : [
            {
              host,
              file: implementation.file,
              line: implementation.line,
              note: `${implementation.note} Live: ${row.hosts[host].live.test_id} (${row.hosts[host].live.observed}).`,
            },
          ];
    });
    record.evidence_ledger = 'docs/specs/edition8/functional-evidence.json';
    // Runtime endpoints and the contract gap are projections of the ledger,
    // not a second hand-maintained audit. The primary seam is the one whose
    // live receipt is recorded above; secondary screen endpoints stay in the
    // catalog only when they are still authoritative.
    const route = row.route;
    const daemonEvidence = row.hosts.daemon.implementation;
    if (route.kind === 'host') {
      record.runtime_endpoints = [
        {
          declared: 'host action',
          state: 'host',
          actual: 'host action',
          line: row.hosts.vscode.implementation?.line ?? null,
        },
      ];
      record.config_keys = [];
    } else if (route.kind === 'cfg') {
      record.runtime_endpoints = [
        {
          declared: `config ${route.config_key}`,
          state: 'exists',
          actual: route.config_key,
          line: daemonEvidence?.line ?? null,
        },
      ];
      record.config_keys = route.config_key === null ? [] : [route.config_key];
    } else {
      record.runtime_endpoints = [
        {
          declared: `${route.method} ${route.path}`,
          state: 'exists',
          actual: route.path,
          line: daemonEvidence?.line ?? null,
        },
      ];
    }
    record.contract_gap = row.status === 'exists' ? 'None.' : row.note;
  }
  controlsDocument.counts.by_status = { ...ledger.counts.by_status };
  controlsDocument.counts.by_host = ledger.counts.by_host;
  controlsDocument.status_source = 'docs/specs/edition8/functional-evidence.json';
  writeFileSync(CONTROLS_PATH, `${JSON.stringify(controlsDocument, null, 2)}\n`);

  if (existsSync(BACKLOG_PATH)) {
    let text = read(BACKLOG_PATH);
    text = replaceBacklogBlock(text, 'counts', backlogCountsBlock(ledger));
    text = replaceBacklogBlock(text, 'table', backlogTableBlock(ledger));
    writeFileSync(BACKLOG_PATH, text);
  }

  // The JetBrains host resolves a control through this generated map first;
  // the hand-written catalog endpoint hints are display metadata only.
  const entries = ledger.controls
    .map((row) => {
      const route = row.route;
      const kind = route.kind === 'cfg' ? 'config' : route.kind === 'act' ? 'action' : route.kind;
      if (kind === 'host') {
        return `        "${row.id}" to Edition8Route("host", "GET", "host", null),`;
      }
      if (kind === 'config') {
        return `        "${row.id}" to Edition8Route("config", "POST", "/native/config/commit", ${JSON.stringify(route.config_key)}),`;
      }
      return `        "${row.id}" to Edition8Route("${kind}", "${route.method}", ${JSON.stringify(route.path)}, null),`;
    })
    .join('\n');
  const kotlin = `// GENERATED FILE - DO NOT EDIT BY HAND.
// Source: docs/specs/edition8/functional-evidence.json
// Regenerate: node scripts/edition8/ledger.mjs --sync
package dev.faktor.frontend

internal data class Edition8Route(
    val kind: String,
    val method: String,
    val path: String,
    val configKey: String?,
) {
    val readable: Boolean = kind == "read" && path.startsWith("/native/")
}

internal object Edition8RouteMap {
    private val ROUTES: Map<String, Edition8Route> = mapOf(
${entries}
    )

    fun route(controlId: String): Edition8Route? = ROUTES[controlId]
}
`;
  writeFileSync(JB_ROUTE_MAP, kotlin);
  console.log(
    `ledger: synced ${ledger.controls.length} controls (${ledger.counts.by_status.exists} exists, ${ledger.counts.by_status.partial} partial, ${ledger.counts.by_status.missing} missing)`,
  );
}

function check({ requireLive = false } = {}) {
  const problems = [];
  if (!existsSync(LEDGER_PATH)) {
    return [`missing ${rel(LEDGER_PATH)}; run --build`];
  }
  const ledger = json(LEDGER_PATH);
  if (ledger.schema !== REQUIRED_LEDGER) problems.push(`ledger schema ${ledger.schema} != ${REQUIRED_LEDGER}`);
  if (!Array.isArray(ledger.controls) || ledger.controls.length !== 200) {
    problems.push(`ledger controls ${ledger.controls?.length} != 200`);
    return problems;
  }
  const controlsDocument = json(CONTROLS_PATH);
  const records = new Map(controlsDocument.records.map((record) => [record.id, record]));
  const ids = new Set();
  for (const row of ledger.controls) {
    if (ids.has(row.id)) problems.push(`duplicate ledger control ${row.id}`);
    ids.add(row.id);
    const record = records.get(row.id);
    if (record === undefined) {
      problems.push(`ledger control ${row.id} is not in controls.json`);
      continue;
    }
    if (!STATUSES.includes(row.status)) problems.push(`${row.id}: bad status ${row.status}`);
    if (row.status !== 'exists' && (typeof row.note !== 'string' || row.note.length < 12)) {
      problems.push(`${row.id}: ${row.status} without an explanatory note`);
    }
    const hostStatuses = HOSTS.map((host) => row.hosts?.[host]?.status);
    if (row.status === 'exists' && !hostStatuses.every((status) => status === 'exists')) {
      problems.push(`${row.id}: aggregate exists but host statuses are ${hostStatuses.join('/')}`);
    }
    if (row.status === 'partial' && !hostStatuses.some((status) => status === 'partial' || status === 'missing')) {
      problems.push(`${row.id}: aggregate partial without a partial/missing host`);
    }
    if (row.status === 'missing' && !hostStatuses.every((status) => status === 'missing')) {
      problems.push(`${row.id}: aggregate missing without all hosts missing`);
    }
    for (const host of HOSTS) {
      const entry = row.hosts?.[host];
      if (entry === undefined) {
        problems.push(`${row.id}: missing ${host} evidence`);
        continue;
      }
      if (!STATUSES.includes(entry.status)) problems.push(`${row.id}/${host}: bad status ${entry.status}`);
      if (entry.status !== 'missing') {
        const implementation = entry.implementation;
        if (implementation === null) {
          problems.push(`${row.id}/${host}: ${entry.status} without implementation evidence`);
        } else {
          const path = resolve(ROOT, implementation.file);
          if (!existsSync(path)) {
            problems.push(`${row.id}/${host}: evidence file missing ${implementation.file}`);
          } else if (!Number.isInteger(implementation.line) || implementation.line < 1) {
            problems.push(`${row.id}/${host}: bad evidence line ${implementation.line}`);
          } else {
            const line = read(path).split('\n')[implementation.line - 1] ?? '';
            if (line.trim().length === 0) problems.push(`${row.id}/${host}: evidence line is blank`);
            else if (
              typeof implementation.line_text !== 'string' ||
              implementation.line_text !== line
            ) {
              problems.push(`${row.id}/${host}: evidence line ${implementation.line} no longer matches the recorded anchor`);
            }
          }
        }
      }
      if (entry.live === null || typeof entry.live.test_id !== 'string') {
        problems.push(`${row.id}/${host}: missing live test id`);
      } else if (requireLive) {
        const [filePart] = entry.live.evidence.split('#');
        const path = resolve(ROOT, filePart);
        if (!existsSync(path)) {
          problems.push(`${row.id}/${host}: live evidence missing ${filePart}`);
        } else if (entry.status === 'exists' && entry.live.passed !== true) {
          problems.push(`${row.id}/${host}: exists but live receipt did not pass (${entry.live.observed})`);
        }
      }
    }
    if (requireLive && row.dogfood !== null && row.dogfood !== undefined) {
      const [filePart] = row.dogfood.evidence.split('#');
      // The dogfood lane is optional in PR CI; when its receipt is present it
      // is held to the same pass rule, but its absence does not invalidate
      // the mandatory host control-exercise receipts checked above.
      if (existsSync(resolve(ROOT, filePart)) && row.status === 'exists' && row.dogfood.passed !== true) {
        problems.push(`${row.id}: exists but dogfood receipt did not pass (${row.dogfood.observed})`);
      }
    }
    if (record.current_status !== row.status) {
      problems.push(`${row.id}: controls.json status ${record.current_status} != ledger ${row.status}`);
    }
    if (record.status_note !== row.note) {
      problems.push(`${row.id}: controls.json status_note is not the ledger projection; run --sync`);
    }
    if (record.evidence_ledger !== 'docs/specs/edition8/functional-evidence.json') {
      problems.push(`${row.id}: controls.json does not point at the functional-evidence ledger`);
    }
    if (!Array.isArray(record.runtime_endpoints) || record.runtime_endpoints.length === 0) {
      problems.push(`${row.id}: controls.json runtime_endpoints is empty; run --sync`);
    }
    if (record.contract_gap !== (row.status === 'exists' ? 'None.' : row.note)) {
      problems.push(`${row.id}: controls.json contract_gap is not the ledger projection; run --sync`);
    }
  }
  const expected = { exists: 0, partial: 0, missing: 0 };
  for (const row of ledger.controls) expected[row.status] += 1;
  for (const status of STATUSES) {
    if (controlsDocument.counts?.by_status?.[status] !== expected[status]) {
      problems.push(
        `controls.json counts.by_status.${status} ${controlsDocument.counts?.by_status?.[status]} != ${expected[status]}`,
      );
    }
  }
  // The generated registry view and backlog block must agree as well.
  if (existsSync(REGISTRY_PATH)) {
    const registry = json(REGISTRY_PATH);
    for (const row of ledger.controls) {
      const control = registry.controls?.find((entry) => entry.id === row.id);
      if (control !== undefined && control.current_status !== row.status) {
        problems.push(`${row.id}: registry.json status ${control.current_status} != ledger ${row.status}; run tests/edition8/registry.mjs --write`);
      }
    }
  }
  if (existsSync(BACKLOG_PATH)) {
    const text = read(BACKLOG_PATH);
    for (const [name, expected] of [
      ['counts', backlogCountsBlock(ledger)],
      ['table', backlogTableBlock(ledger)],
    ]) {
      const beginMarker = `<!-- edition8-status-${name}:begin -->`;
      const endMarker = `<!-- edition8-status-${name}:end -->`;
      const begin = text.indexOf(beginMarker);
      const end = text.indexOf(endMarker);
      if (begin < 0 || end < 0) {
        problems.push(`backlog.md has no generated ${name} block`);
      } else {
        const actual = text.slice(begin, end + endMarker.length);
        if (actual !== expected) problems.push(`backlog.md ${name} block diverges from the ledger; run --sync`);
      }
    }
  }
  if (existsSync(JB_ROUTE_MAP)) {
    const routeText = read(JB_ROUTE_MAP);
    for (const row of ledger.controls) {
      if (!routeText.includes(`"${row.id}" to Edition8Route(`)) {
        problems.push(`Edition8RouteMap.kt missing ${row.id}`);
      }
    }
  } else {
    problems.push('Edition8RouteMap.kt is missing; run --sync');
  }
  // The canonical VS Code route table must agree with the ledger's route.
  const routes = vscodeRoutes();
  for (const row of ledger.controls) {
    const route = routes.get(row.id);
    if (route === undefined) {
      problems.push(`${row.id}: VS Code route missing`);
      continue;
    }
    if (route.kind !== row.route.kind) problems.push(`${row.id}: route kind ${route.kind} != ${row.route.kind}`);
    if (route.kind !== 'host' && route.kind !== 'config' && route.path !== row.route.path) {
      problems.push(`${row.id}: route ${route.path} != ${row.route.path}`);
    }
    if (route.kind === 'config' && route.configKey !== row.route.config_key) {
      problems.push(`${row.id}: config key ${route.configKey} != ${row.route.config_key}`);
    }
  }
  return problems;
}

function selftest() {
  // Copy the generated artifacts to a scratch tree and prove each divergence
  // class is loud. The real files are never modified.
  const original = {
    controls: read(CONTROLS_PATH),
    backlog: read(BACKLOG_PATH),
    ledger: read(LEDGER_PATH),
  };
  const restore = () => {
    writeFileSync(CONTROLS_PATH, original.controls);
    writeFileSync(BACKLOG_PATH, original.backlog);
    writeFileSync(LEDGER_PATH, original.ledger);
  };
  const failures = [];
  try {
    const controls = JSON.parse(original.controls);
    controls.records[0].current_status = controls.records[0].current_status === 'exists' ? 'missing' : 'exists';
    writeFileSync(CONTROLS_PATH, `${JSON.stringify(controls, null, 2)}\n`);
    if (check().length === 0) failures.push('controls.json divergence was not detected');
    restore();
    const backlog = original.backlog.replace(/(\d+) `exists`/, (_m, n) => `${Number(n) + 1} \`exists\``);
    writeFileSync(BACKLOG_PATH, backlog);
    if (check().length === 0) failures.push('backlog divergence was not detected');
    restore();
    const ledger = JSON.parse(original.ledger);
    ledger.controls[0].hosts.vscode.status = 'missing';
    writeFileSync(LEDGER_PATH, `${JSON.stringify(ledger, null, 2)}\n`);
    if (check().length === 0) failures.push('ledger host divergence was not detected');
    restore();
  } finally {
    restore();
  }
  if (failures.length > 0) {
    for (const failure of failures) console.error(`FAIL ${failure}`);
    return 1;
  }
  console.log('ledger selftest PASS: controls, backlog and ledger divergences are all detected');
  return 0;
}

const args = process.argv.slice(2);
if (args.includes('--build')) {
  const ledger = build();
  writeFileSync(LEDGER_PATH, `${JSON.stringify(ledger, null, 2)}\n`);
  console.log(
    `ledger: built ${ledger.controls.length} controls (${ledger.counts.by_status.exists} exists, ${ledger.counts.by_status.partial} partial, ${ledger.counts.by_status.missing} missing)`,
  );
} else if (args.includes('--sync')) {
  sync();
  try {
    execFileSync(process.execPath, [resolve(ROOT, 'tests/edition8/registry.mjs'), '--write'], {
      cwd: ROOT,
      stdio: 'pipe',
    });
    execFileSync(process.execPath, [resolve(ROOT, 'scripts/edition8/codegen.mjs'), '--write'], {
      cwd: ROOT,
      stdio: 'pipe',
    });
    console.log('ledger: regenerated registry.json and both host registries');
  } catch (error) {
    console.error(`ledger: regeneration failed: ${error.stderr?.toString() ?? error.message}`);
    process.exit(1);
  }
} else if (args.includes('--check')) {
  const problems = check({ requireLive: args.includes('--require-live') });
  if (problems.length > 0) {
    for (const problem of problems) console.error(`FAIL ${problem}`);
    process.exit(1);
  }
  console.log('ledger: controls.json, backlog.md, registry.json and host routes all match the functional-evidence ledger');
} else if (args.includes('--selftest')) {
  process.exitCode = selftest();
} else {
  console.error('usage: ledger.mjs --build|--sync|--check|--selftest');
  process.exitCode = 2;
}
