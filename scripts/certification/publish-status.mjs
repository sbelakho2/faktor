#!/usr/bin/env node
// Exact-SHA GitHub commit-status publication for the trusted workflow
// (audit 23).
//
// Why this exists: the trusted pipeline's result must be visible on the
// EXACT shipped commit even when a lane or the certificate step fails. The
// trusted workflow's `status-publish` step runs with
// `when.status: [success, failure]` after the certificate step and calls
// this script, which posts a commit status for `CI_COMMIT_SHA` under the
// registered trusted context. Failures before any step can run (workflow
// config/startup errors) are covered by Woodpecker's own forge integration,
// which publishes the pipeline's error status for the same context; the
// certify.sh forge check re-verifies conclusiveness for the exact SHA
// through the GitHub API.
//
// Fail-closed: a missing token, malformed SHA/repo/state or a non-2xx API
// response is a typed nonzero exit — a run can never silently skip the
// status publication.
//
// Usage:
//   node scripts/certification/publish-status.mjs publish \
//     --repo owner/name --sha <40-hex> --state success|failure|error|pending \
//     --context ci/woodpecker/push/trusted --description "..." \
//     [--target-url https://...] [--api https://api.github.com]
//   node scripts/certification/publish-status.mjs selftest
//
// Token: GITHUB_STATUS_TOKEN, then GH_TOKEN, then GITHUB_TOKEN. The token is
// never printed or embedded in an error message.

const API_DEFAULT = 'https://api.github.com';
const STATES = new Set(['success', 'failure', 'error', 'pending']);
const SHA_RE = /^[0-9a-f]{40}$/;
const REPO_RE = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;
const MAX_DESCRIPTION = 140;

export class StatusError extends Error {
  constructor(code, detail) {
    super(`github-status-${code}: ${detail}`);
    this.code = code;
  }
}

function fail(code, detail) {
  throw new StatusError(code, detail);
}

function argValue(args, name, fallback = undefined) {
  const at = args.indexOf(name);
  if (at === -1) return fallback;
  if (at + 1 >= args.length) fail('args', `${name} requires a value`);
  return args[at + 1];
}

function tokenFromEnv(env) {
  return env.GITHUB_STATUS_TOKEN || env.GH_TOKEN || env.GITHUB_TOKEN || '';
}

export function validate({ repo, sha, state, context, description, targetUrl }) {
  if (!repo || !REPO_RE.test(repo)) fail('repo', `invalid repository ${JSON.stringify(repo)}`);
  if (!sha || !SHA_RE.test(sha)) {
    fail('sha', '--sha must be the exact 40-lowercase-hex commit, never a branch or tag');
  }
  if (!STATES.has(state)) fail('state', `--state must be one of ${[...STATES].join(', ')}`);
  if (!context || /\s/.test(context)) fail('context', '--context must be a non-empty status context');
  if (!description || description.length > MAX_DESCRIPTION) {
    fail('description', `--description must be 1..${MAX_DESCRIPTION} characters`);
  }
  if (targetUrl && !/^https:\/\//.test(targetUrl)) {
    fail('target-url', '--target-url must be an https URL when set');
  }
}

export async function publish({ api, repo, sha, state, context, description, targetUrl, token, fetchImpl = fetch }) {
  validate({ repo, sha, state, context, description, targetUrl });
  if (!token) fail('token-missing', 'GITHUB_STATUS_TOKEN/GH_TOKEN/GITHUB_TOKEN is required (fail closed)');
  const body = { state, context, description };
  if (targetUrl) body.target_url = targetUrl;
  const response = await fetchImpl(`${api.replace(/\/+$/, '')}/repos/${repo}/statuses/${sha}`, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${token}`,
      Accept: 'application/vnd.github+json',
      'Content-Type': 'application/json',
      'User-Agent': 'faktor-certification',
    },
    body: JSON.stringify(body),
  });
  const text = await response.text();
  if (response.status !== 200 && response.status !== 201) {
    fail('publish-failed', `HTTP ${response.status} from the statuses API: ${text.slice(0, 200)}`);
  }
  console.log(`github-status-published context=${context} state=${state} sha=${sha}`);
  return { status: response.status, body: text };
}

// ------------------------------------------------- platform aggregation (P0-1)
//
// `ci/faktor/trusted-certified` is an AGGREGATE: it may only become success
// when the linux, darwin AND windows per-platform certificates are success
// for the SAME exact SHA and tree, all bound to ONE pipeline execution (the
// same workflow generation/config), each published by its own platform step.
// A per-platform status description carries `tree=<40hex> run=<pipeline>:<platform>`;
// the run binding makes a duplicate/forged copy (e.g. the linux execution
// publishing the windows context) fail, a run mismatch across platforms
// (a stale certificate from an older pipeline) fails, and missing/stale/
// failing platforms can never aggregate green.

export const AGGREGATE_CONTEXTS = {
  linux: 'ci/faktor/trusted-certified-linux',
  darwin: 'ci/faktor/trusted-certified-darwin',
  windows: 'ci/faktor/trusted-certified-windows',
};
const TREE_RE = /(?:^|\s)tree=([0-9a-f]{40})(?:\s|$)/;
const RUN_RE = /(?:^|\s)run=([^\s]+)(?:\s|$)/;

export function platformFacts(status) {
  const description = String(status.description || '');
  const tree = TREE_RE.exec(description);
  const run = RUN_RE.exec(description);
  return { tree: tree ? tree[1] : null, run: run ? run[1] : null };
}

/// `statuses` is the newest-first commit-status list for ONE sha.
export function aggregateVerdict({ statuses, tree }) {
  const reasons = [];
  const runs = new Set();
  let pipelineRun = null;
  for (const [platform, context] of Object.entries(AGGREGATE_CONTEXTS)) {
    const entry = statuses.find((status) => status.context === context);
    if (!entry) {
      reasons.push(`${platform}=missing`);
      continue;
    }
    if (entry.state !== 'success') {
      reasons.push(`${platform}=${entry.state}`);
      continue;
    }
    const { tree: statusTree, run } = platformFacts(entry);
    if (statusTree !== tree) {
      reasons.push(`${platform}=tree-mismatch`);
      continue;
    }
    if (!run || !run.endsWith(`:${platform}`)) {
      reasons.push(`${platform}=run-unbound`);
      continue;
    }
    if (runs.has(run)) {
      reasons.push(`${platform}=duplicate-run`);
      continue;
    }
    runs.add(run);
    // Same workflow generation/config: every matrix axis of one pipeline
    // shares the pipeline number, so a certificate from a different
    // pipeline (even for the same tree) is stale evidence and refuses.
    const prefix = run.slice(0, run.lastIndexOf(':'));
    if (pipelineRun === null) pipelineRun = prefix;
    else if (pipelineRun !== prefix) reasons.push(`${platform}=run-mismatch`);
  }
  if (reasons.length > 0) {
    return { ok: false, reason: `platform certificates incomplete: ${reasons.join(', ')}` };
  }
  return {
    ok: true,
    reason: `linux+darwin+windows certificates passed (tree=${tree} run=${pipelineRun})`,
  };
}

export async function readStatuses({ api, repo, sha, token, fetchImpl = fetch }) {
  if (!token) fail('token-missing', 'GITHUB_STATUS_TOKEN/GH_TOKEN/GITHUB_TOKEN is required (fail closed)');
  const response = await fetchImpl(
    `${api.replace(/\/+$/, '')}/repos/${repo}/commits/${sha}/statuses?per_page=100`,
    {
      headers: {
        Authorization: `Bearer ${token}`,
        Accept: 'application/vnd.github+json',
        'User-Agent': 'faktor-certification',
      },
    },
  );
  const text = await response.text();
  if (response.status !== 200) {
    fail('read-failed', `HTTP ${response.status} from the statuses API: ${text.slice(0, 200)}`);
  }
  return JSON.parse(text).map((entry) => ({
    context: entry.context,
    state: entry.state,
    description: entry.description || '',
  }));
}

/// Poll the three platform contexts (bounded), then publish the aggregate
/// verdict. Non-success exits 1 AFTER publishing: the pipeline must be red
/// and the exact-SHA status must be conclusive either way.
export async function aggregate({
  api,
  repo,
  sha,
  tree,
  token,
  pollSeconds = 0,
  intervalSeconds = 15,
  context = 'ci/faktor/trusted-certified',
  fetchImpl = fetch,
  sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
}) {
  if (!SHA_RE.test(tree)) fail('tree', '--tree must be the exact 40-lowercase-hex tree');
  const deadline = Date.now() + pollSeconds * 1000;
  let verdict;
  for (;;) {
    const statuses = await readStatuses({ api, repo, sha, token, fetchImpl });
    verdict = aggregateVerdict({ statuses, tree });
    if (verdict.ok || Date.now() >= deadline) break;
    await sleep(intervalSeconds * 1000);
  }
  await publish({
    api,
    repo,
    sha,
    state: verdict.ok ? 'success' : 'failure',
    context,
    description: verdict.reason.slice(0, MAX_DESCRIPTION),
    token,
    fetchImpl,
  });
  if (!verdict.ok) {
    console.error(`github-status-aggregate-refused: ${verdict.reason}`);
    process.exitCode = 1;
  }
  return verdict;
}

// ------------------------------------------------------------------ selftest

async function selftest() {
  const { createServer } = await import('node:http');
  let failures = 0;
  const check = (name, ok, detail = '') => {
    if (ok) console.log(`selftest ok: ${name}`);
    else {
      console.error(`selftest FAIL: ${name}${detail ? ` (${detail})` : ''}`);
      failures += 1;
    }
  };

  const requests = [];
  let statusesFixture = [];
  const server = createServer((req, res) => {
    let data = '';
    req.on('data', (chunk) => (data += chunk));
    req.on('end', () => {
      requests.push({ method: req.method, url: req.url, auth: req.headers.authorization, body: data });
      if (req.method === 'GET') {
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end(JSON.stringify(statusesFixture));
        return;
      }
      if (req.url.includes('reject') || data.includes('reject')) {
        res.writeHead(422, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ message: 'Validation Failed' }));
        return;
      }
      res.writeHead(201, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ id: 1 }));
    });
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const api = `http://127.0.0.1:${server.address().port}`;
  const sha = 'a'.repeat(40);

  // A missing token refuses WITHOUT any network call.
  const before = requests.length;
  try {
    await publish({ api, repo: 'acme/widgets', sha, state: 'success', context: 'ci/x', description: 'd', token: '' });
    check('missing token refuses', false);
  } catch (error) {
    check('missing token refuses', String(error.message).includes('github-status-token-missing'));
  }
  check('missing token makes no request', requests.length === before);

  // A malformed SHA (branch/tag/prefix) refuses without any network call.
  for (const bad of ['main', 'abc123', 'A'.repeat(40), 'a'.repeat(39)]) {
    try {
      await publish({ api, repo: 'acme/widgets', sha: bad, state: 'success', context: 'ci/x', description: 'd', token: 't' });
      check(`malformed sha ${JSON.stringify(bad)} refuses`, false);
    } catch (error) {
      check(`malformed sha ${JSON.stringify(bad)} refuses`, String(error.message).includes('github-status-sha'));
    }
  }

  // Exact-SHA success publication carries the exact path/body/auth.
  await publish({
    api,
    repo: 'acme/widgets',
    sha,
    state: 'success',
    context: 'ci/woodpecker/push/trusted',
    description: 'Faktor trusted certification passed',
    targetUrl: 'https://ci.example/pipelines/1',
    token: 'selftest-token',
  });
  const posted = requests.at(-1);
  check('posts to /repos/{repo}/statuses/{sha}', posted.url === `/repos/acme/widgets/statuses/${sha}`, posted.url);
  check('posts with bearer auth', posted.auth === 'Bearer selftest-token');
  const parsed = JSON.parse(posted.body);
  check(
    'body binds state/context/description/target_url',
    parsed.state === 'success' &&
      parsed.context === 'ci/woodpecker/push/trusted' &&
      parsed.description === 'Faktor trusted certification passed' &&
      parsed.target_url === 'https://ci.example/pipelines/1',
    posted.body,
  );

  // A failed run publishes failure for the same exact SHA (lane failures are
  // not silently omitted).
  await publish({
    api,
    repo: 'acme/widgets',
    sha,
    state: 'failure',
    context: 'ci/woodpecker/push/trusted',
    description: 'Faktor trusted certification failed',
    token: 'selftest-token',
  });
  check('failure state publishes for the same SHA', JSON.parse(requests.at(-1).body).state === 'failure');

  // A non-2xx API answer is a typed refusal.
  try {
    await publish({ api, repo: 'acme/widgets', sha, state: 'success', context: 'ci/reject', description: 'd', token: 't' });
    check('non-2xx refuses', false);
  } catch (error) {
    check('non-2xx refuses', String(error.message).includes('github-status-publish-failed'));
  }

  // Validation refusals never touch the network.
  const netBefore = requests.length;
  for (const [name, params] of [
    ['invalid state', { state: 'green' }],
    ['invalid repo', { repo: 'not a repo' }],
    ['empty context', { context: '' }],
    ['oversized description', { description: 'x'.repeat(141) }],
    ['non-https target', { targetUrl: 'http://ci.example/1' }],
  ]) {
    try {
      await publish({ api, repo: 'acme/widgets', sha, state: 'success', context: 'ci/x', description: 'd', token: 't', ...params });
      check(`${name} refuses`, false);
    } catch (error) {
      check(`${name} refuses`, String(error.message).startsWith('github-status-'));
    }
  }
  check('validation refusals make no request', requests.length === netBefore);

  // ---- P0-1: the aggregate table ---------------------------------------
  const tree = 'b'.repeat(40);
  const otherTree = 'c'.repeat(40);
  const okStatus = (platform) => ({
    context: AGGREGATE_CONTEXTS[platform],
    state: 'success',
    description: `tree=${tree} run=700:${platform}`,
  });
  check(
    'aggregate: linux+darwin+windows pass -> success',
    aggregateVerdict({ statuses: [okStatus('linux'), okStatus('darwin'), okStatus('windows')], tree }).ok,
  );
  check(
    'aggregate: missing windows -> non-success',
    !aggregateVerdict({ statuses: [okStatus('linux'), okStatus('darwin')], tree }).ok,
  );
  check(
    'aggregate: failing windows -> non-success',
    !aggregateVerdict({
      statuses: [okStatus('linux'), okStatus('darwin'), { ...okStatus('windows'), state: 'failure' }],
      tree,
    }).ok,
  );
  check(
    'aggregate: stable sha with a stale darwin tree -> non-success',
    !aggregateVerdict({
      statuses: [okStatus('linux'), { ...okStatus('darwin'), description: `tree=${otherTree} run=700:darwin` }, okStatus('windows')],
      tree,
    }).ok,
  );
  check(
    'aggregate: duplicate linux masquerading as windows -> non-success',
    !aggregateVerdict({
      statuses: [okStatus('linux'), okStatus('darwin'), { ...okStatus('windows'), description: `tree=${tree} run=700:linux` }],
      tree,
    }).ok,
  );
  check(
    'aggregate: all pass with differing trees cannot happen (tree-mismatch) -> non-success',
    !aggregateVerdict({
      statuses: [
        { ...okStatus('linux'), description: `tree=${otherTree} run=700:linux` },
        okStatus('darwin'),
        okStatus('windows'),
      ],
      tree,
    }).ok,
  );
  check(
    'aggregate: platforms from DIFFERENT pipeline runs (stale certificate) -> non-success',
    !aggregateVerdict({
      statuses: [
        okStatus('linux'),
        { ...okStatus('darwin'), description: `tree=${tree} run=701:darwin` },
        { ...okStatus('windows'), description: `tree=${tree} run=702:windows` },
      ],
      tree,
    }).ok,
  );
  check(
    'aggregate: windows is a REQUIRED platform (mutation witness: removing the lookup fails here)',
    Object.prototype.hasOwnProperty.call(AGGREGATE_CONTEXTS, 'windows') &&
      aggregateVerdict({ statuses: [okStatus('linux'), okStatus('darwin')], tree }).reason.includes('windows'),
  );

  // Polling: empty at first, complete after one sleep -> aggregate success,
  // and the published context/state are the aggregate ones.
  statusesFixture = [];
  const sleeps = [];
  const polled = await aggregate({
    api,
    repo: 'acme/widgets',
    sha,
    tree,
    token: 'selftest-token',
    pollSeconds: 5,
    intervalSeconds: 1,
    sleep: async (ms) => {
      sleeps.push(ms);
      statusesFixture = [okStatus('linux'), okStatus('darwin'), okStatus('windows')];
    },
  });
  check('aggregate polling reaches success', polled.ok && sleeps.length === 1);
  const aggPost = JSON.parse(requests.at(-1).body);
  check(
    'aggregate publishes the trusted context as success',
    aggPost.context === 'ci/faktor/trusted-certified' && aggPost.state === 'success',
    requests.at(-1).body,
  );
  check(
    'platform statuses are read from the exact-sha statuses API',
    requests.some((r) => r.method === 'GET' && r.url === `/repos/acme/widgets/commits/${sha}/statuses?per_page=100`),
  );

  await new Promise((resolve) => server.close(resolve));
  if (failures > 0) {
    console.error(`publish-status selftest: FAIL (${failures})`);
    process.exit(1);
  }
  console.log('publish-status selftest: PASS');
}

const isMain = process.argv[1] && process.argv[1].endsWith('publish-status.mjs');
if (isMain) {
  const args = process.argv.slice(2);
  const command = args[0];
  try {
    if (command === 'selftest') {
      await selftest();
    } else if (command === 'publish') {
      await publish({
        api: argValue(args, '--api', process.env.GITHUB_STATUS_API || API_DEFAULT),
        repo: argValue(args, '--repo', process.env.CI_REPO || ''),
        sha: argValue(args, '--sha', process.env.CI_COMMIT_SHA || ''),
        state: argValue(args, '--state', ''),
        context: argValue(args, '--context', 'ci/woodpecker/push/trusted'),
        description: argValue(args, '--description', ''),
        targetUrl: argValue(args, '--target-url', process.env.CI_PIPELINE_URL || ''),
        token: tokenFromEnv(process.env),
      });
    } else if (command === 'aggregate') {
      await aggregate({
        api: argValue(args, '--api', process.env.GITHUB_STATUS_API || API_DEFAULT),
        repo: argValue(args, '--repo', process.env.CI_REPO || ''),
        sha: argValue(args, '--sha', process.env.CI_COMMIT_SHA || ''),
        tree: argValue(args, '--tree', ''),
        token: tokenFromEnv(process.env),
        pollSeconds: Number(argValue(args, '--poll-seconds', '0')),
        intervalSeconds: Number(argValue(args, '--interval-seconds', '15')),
        context: argValue(args, '--context', 'ci/faktor/trusted-certified'),
      });
    } else {
      fail('args', 'usage: publish-status.mjs publish|aggregate|selftest');
    }
  } catch (error) {
    console.error(error instanceof StatusError ? error.message : `github-status-internal: ${error.message}`);
    process.exit(1);
  }
}
