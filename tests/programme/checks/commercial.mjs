#!/usr/bin/env node
// Commercial capability matrix: prove every commercial action reaches a real
// server handler AND a real service seam, and that every IDE client consumes
// the same route. This is the commercial-specific complement to the generic
// wiring matrix (which proves a route has *some* client).
//
// Checks:
//   1. action -> route: every commercial action names a discovered native
//      route (method + path), so a renamed/removed route is loud;
//   2. action -> handler -> service: the named handler exists, lives in the
//      named server file, and its body calls the named billing service
//      method (a UI-only or inert handler cannot pass);
//   3. action -> client: BOTH Faktor-owned IDE clients (VS Code TypeScript
//      and JetBrains Kotlin) contain the route literal and the client method;
//   4. setting -> consumer: every `[billing]` setting is read through a
//      `self.<key>` / `config_billing.<key>` / `billing.<key>` expression
//      outside its declaration-only shape.
//
// Planted violations (selftest): a missing route, a handler with no service
// call, a client missing the route, and a setting with no consumer are each
// caught; the corrected fixture is clean.

import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { discoverNativeRoutes } from '../../../scripts/discover-capabilities.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
export const ROOT = resolve(HERE, '..', '..', '..');

/** Every commercial action, its route, handler and service seam. */
export const COMMERCIAL_ACTIONS = [
  {
    id: 'usage.read',
    method: 'GET',
    path: '/native/usage',
    handler: 'billing_usage',
    server: 'crates/server/src/native/billing.rs',
    service: /service\.(usage_page|fold|credit_balance)\s*\(/,
    client: 'billingUsage',
  },
  {
    id: 'usage.export',
    method: 'GET',
    path: '/native/usage/export',
    handler: 'native_usage_export',
    server: 'crates/server/src/native/billing.rs',
    service: /service\.usage_page\s*\(/,
    client: 'usageExport',
    cli: {
      file: 'crates/cli/src/billing_cli.rs',
      marker: 'BillingAction::Usage',
      via: ['usage_page'],
      service: /\.usage_page\s*\(/,
    },
  },
  {
    id: 'entitlements.read',
    method: 'GET',
    path: '/native/entitlements',
    handler: 'native_entitlements',
    server: 'crates/server/src/native/billing.rs',
    service: /service\.entitlement_snapshot\s*\(/,
    client: 'entitlements',
  },
  {
    id: 'value_entitlements.read',
    method: 'GET',
    path: '/native/value-entitlements',
    handler: 'native_value_entitlements',
    server: 'crates/server/src/native/value.rs',
    service: /service\.value_entitlements\s*\(/,
    client: 'valueEntitlements',
  },
  {
    id: 'credits.grant',
    method: 'POST',
    path: '/native/credits/grant',
    handler: 'native_credits_grant',
    server: 'crates/server/src/native/billing.rs',
    service: /service\.grant_credits\s*\(/,
    client: 'grantCredits',
    cli: {
      file: 'crates/cli/src/billing_cli.rs',
      marker: 'BillingAction::Credits',
      via: ['render_impl'],
      service: /\.grant_credits\s*\(/,
    },
  },
  {
    id: 'outcomes.read',
    method: 'GET',
    path: '/native/outcomes',
    handler: 'native_outcomes',
    server: 'crates/server/src/native/outcomes.rs',
    service: /service\.check_value_entitlement\s*\(/,
    via: ['authorize_outcomes_organization'],
    client: 'outcomes',
  },
  {
    id: 'billing.overview',
    method: 'GET',
    path: '/native/billing/overview',
    handler: 'native_billing_overview',
    server: 'crates/server/src/native/billing.rs',
    service: /service\.overview\s*\(/,
    client: 'billingOverview',
    cli: {
      file: 'crates/cli/src/billing_cli.rs',
      marker: 'BillingAction::Overview',
      via: ['render_impl'],
      service: /\.overview\s*\(/,
    },
  },
  {
    id: 'billing.subscription',
    method: 'POST',
    path: '/native/billing/subscription',
    handler: 'native_billing_subscription',
    server: 'crates/server/src/native/billing.rs',
    service: /service\.(change_subscription|cancel_subscription)\s*\(/,
    client: 'changeSubscription',
    cli: {
      file: 'crates/cli/src/billing_cli.rs',
      marker: 'BillingAction::Subscription',
      via: ['render_impl'],
      service: /\.(change_subscription|cancel_subscription)\s*\(/,
    },
  },
];

/** Every `[billing]` setting that must have a real read path. */
export const COMMERCIAL_SETTINGS = [
  'enabled',
  'database',
  'organization',
  'account',
  'account_name',
  'managed',
  'default_plan',
  'plans',
  'managed_providers',
  'subscription',
  'report',
];

function read(root, rel) {
  const path = join(root, rel);
  return existsSync(path) ? readFileSync(path, 'utf8') : null;
}

function walk(root, rel, extensions = /\.(rs|ts|kt)$/) {
  const base = join(root, rel);
  const out = [];
  if (!existsSync(base)) return out;
  const visit = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      if (['target', 'node_modules', 'out', 'build', '.git', '.gradle'].includes(entry.name)) {
        continue;
      }
      const path = join(dir, entry.name);
      if (entry.isDirectory()) visit(path);
      else if (extensions.test(entry.name) && !/test/i.test(entry.name)) out.push(path);
    }
  };
  visit(base);
  return out;
}

function bodyOf(text, handler, maxBytes = 24_000) {
  const declaration = new RegExp(`(?:pub(?:\\([^)]*\\))?\\s+)?(?:async\\s+)?fn\\s+${handler}\\s*\\(`).exec(
    text,
  );
  if (!declaration) return null;
  const next = /\n(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+[A-Za-z_][A-Za-z0-9_]*\s*\(/g;
  next.lastIndex = declaration.index + declaration[0].length;
  const found = next.exec(text);
  const end = found === null ? Math.min(text.length, declaration.index + maxBytes) : found.index;
  return text.slice(declaration.index, end);
}

function routeKey(method, path) {
  return `${method.toUpperCase()} ${path}`;
}

/**
 * Scan one tree. `actions`/`settings` may be overridden by the selftest;
 * production uses the exported constants.
 */
export function scan(root = ROOT, { actions = COMMERCIAL_ACTIONS, settings = COMMERCIAL_SETTINGS } = {}) {
  const routes = discoverNativeRoutes(root);
  const routeIndex = new Map(routes.map((route) => [routeKey(route.method, route.path), route]));
  const serverFiles = walk(root, 'crates/server/src');
  const serverSources = new Map(
    serverFiles.map((file) => [relative(root, file), readFileSync(file, 'utf8')]),
  );
  const vscode = read(root, 'apps/vscode/src/nativeClient.ts') ?? '';
  const jetbrains =
    read(root, 'apps/jetbrains/backend/src/main/kotlin/dev/faktor/backend/NativeClient.kt') ?? '';
  const settingsText = [
    ...walk(root, 'crates'),
    ...walk(root, 'apps'),
  ]
    .map((file) => readFileSync(file, 'utf8'))
    .join('\n');
  const problems = [];

  for (const action of actions) {
    const key = routeKey(action.method, action.path);
    const route = routeIndex.get(key);
    if (route === undefined) {
      problems.push(`commercial action ${action.id}: route ${key} is not registered`);
    }
    const serverText = serverSources.get(action.server) ?? '';
    const body = bodyOf(serverText, action.handler);
    if (body === null) {
      problems.push(
        `commercial action ${action.id}: handler ${action.handler} is absent from ${action.server}`,
      );
    } else {
      const delegated = (action.via ?? []).map((name) => bodyOf(serverText, name) ?? '');
      if (![body, ...delegated].some((slice) => action.service.test(slice))) {
        problems.push(
          `commercial action ${action.id}: handler ${action.handler} never calls its billing service seam`,
        );
      }
    }
    const clientPattern = new RegExp(`\\b${action.client}\\s*\\(`);
    if (!vscode.includes(action.path)) {
      problems.push(`commercial action ${action.id}: VS Code client does not consume ${action.path}`);
    }
    if (!clientPattern.test(vscode)) {
      problems.push(`commercial action ${action.id}: VS Code client lacks ${action.client}()`);
    }
    if (!jetbrains.includes(action.path)) {
      problems.push(`commercial action ${action.id}: JetBrains client does not consume ${action.path}`);
    }
    if (!clientPattern.test(jetbrains)) {
      problems.push(`commercial action ${action.id}: JetBrains client lacks ${action.client}()`);
    }
    if (action.cli !== undefined) {
      const cliText = read(root, action.cli.file) ?? '';
      if (!cliText.includes(action.cli.marker)) {
        problems.push(
          `commercial action ${action.id}: CLI ${action.cli.file} lacks ${action.cli.marker}`,
        );
      }
      const cliBody = bodyOf(cliText, 'render');
      const cliDelegated = (action.cli.via ?? []).map((name) => bodyOf(cliText, name) ?? '');
      if (
        cliBody === null ||
        ![cliBody, ...cliDelegated].some((slice) => action.cli.service.test(slice))
      ) {
        problems.push(
          `commercial action ${action.id}: CLI render never calls its billing service seam`,
        );
      }
    }
  }

  for (const setting of settings) {
    const consumer = new RegExp(`(?:self|config_billing|billing)\\.${setting}\\b`);
    if (!consumer.test(settingsText)) {
      problems.push(`commercial setting billing.${setting}: no consumer reads it`);
    }
  }

  return {
    problems,
    warnings: [],
    counts: { actions: actions.length, settings: settings.length, routes: routes.length },
  };
}

/** Planted violations: one per check, then the corrected fixture is clean. */
export function selftest() {
  let failures = 0;
  const check = (name, condition, detail = '') => {
    if (condition) console.log(`commercial selftest ok: ${name}`);
    else {
      failures += 1;
      console.error(`commercial selftest FAIL: ${name}${detail ? ` (${detail})` : ''}`);
    }
  };
  const fixture = (files) => {
    const root = mkdtempSync(join(tmpdir(), 'faktor-commercial-'));
    for (const [rel, text] of Object.entries(files)) {
      mkdirSync(dirname(join(root, rel)), { recursive: true });
      writeFileSync(join(root, rel), text);
    }
    return root;
  };
  const action = {
    id: 'planted.read',
    method: 'GET',
    path: '/native/planted',
    handler: 'planted_handler',
    server: 'crates/server/src/native/planted.rs',
    service: /service\.planted_method\s*\(/,
    client: 'plantedRead',
  };
  const clean = {
    'crates/server/src/api.rs':
      'fn routes(r: Router) -> Router { r.route("/native/planted", get(planted_handler)) }\n',
    'crates/server/src/native/planted.rs':
      'pub(crate) async fn planted_handler(state: State) -> Response { let _ = state.deps.billing.as_ref(); service.planted_method() }\n',
    'apps/vscode/src/nativeClient.ts':
      "export class NativeClient { async plantedRead() { return this.request('GET', '/native/planted'); } }\n",
    'apps/jetbrains/backend/src/main/kotlin/dev/faktor/backend/NativeClient.kt':
      'class NativeClient { fun plantedRead(): String = request("GET", "/native/planted") }\n',
    'crates/cli/src/config/planted.rs': 'impl Cfg { fn read(&self) { let _ = self.planted_field; } }\n',
  };

  const root = fixture(clean);
  const pass = scan(root, { actions: [action], settings: ['planted_field'] });
  check('clean commercial fixture passes', pass.problems.length === 0, pass.problems.join('; '));

  const missingRoute = fixture({ ...clean, 'crates/server/src/api.rs': 'fn routes() {}\n' });
  const routeResult = scan(missingRoute, { actions: [action], settings: ['planted_field'] });
  check(
    'missing commercial route is caught',
    routeResult.problems.some((p) => p.includes('/native/planted') && p.includes('not registered')),
    routeResult.problems.join('; '),
  );

  const inertHandler = fixture({
    ...clean,
    'crates/server/src/native/planted.rs':
      'pub(crate) async fn planted_handler() -> Response { wire_status(malformed_body("inert")) }\n',
  });
  const handlerResult = scan(inertHandler, { actions: [action], settings: ['planted_field'] });
  check(
    'handler without a service seam is caught',
    handlerResult.problems.some((p) => p.includes('never calls its billing service seam')),
    handlerResult.problems.join('; '),
  );

  const missingClient = fixture({
    ...clean,
    'apps/vscode/src/nativeClient.ts': 'export class NativeClient {}\n',
  });
  const clientResult = scan(missingClient, { actions: [action], settings: ['planted_field'] });
  check(
    'commercial route without a client is caught',
    clientResult.problems.some((p) => p.includes('VS Code client does not consume')),
    clientResult.problems.join('; '),
  );

  const missingSetting = fixture(clean);
  const settingResult = scan(missingSetting, {
    actions: [action],
    settings: ['planted_field', 'unread_field'],
  });
  check(
    'setting without a consumer is caught',
    settingResult.problems.some((p) => p.includes('billing.unread_field')),
    settingResult.problems.join('; '),
  );

  return failures === 0 ? 0 : 1;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  if (args.includes('--selftest')) process.exitCode = selftest();
  else {
    const result = scan(ROOT);
    for (const warning of result.warnings) console.error(`warning: ${warning}`);
    for (const problem of result.problems) console.error(`problem: ${problem}`);
    console.log(
      `commercial matrix: ${result.counts.actions} actions, ${result.counts.settings} settings, ${result.counts.routes} routes; ${result.problems.length} problems`,
    );
    process.exitCode = result.problems.length === 0 ? 0 : 1;
  }
}
