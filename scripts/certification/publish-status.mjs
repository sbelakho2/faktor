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
  const server = createServer((req, res) => {
    let data = '';
    req.on('data', (chunk) => (data += chunk));
    req.on('end', () => {
      requests.push({ method: req.method, url: req.url, auth: req.headers.authorization, body: data });
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
    } else {
      fail('args', 'usage: publish-status.mjs publish|selftest');
    }
  } catch (error) {
    console.error(error instanceof StatusError ? error.message : `github-status-internal: ${error.message}`);
    process.exit(1);
  }
}
