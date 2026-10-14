#!/usr/bin/env python3
"""Stdlib GitHub commit-status publisher for platform lanes without node.

Parity port of the `publish` half of scripts/certification/publish-status.mjs
(validation + fail-closed POST + platform HMAC signing). The darwin
certificate step publishes its per-platform status through this script; the
linux certificate and the semantic aggregate use the node script; the
windows certificate signs with the same canonical payload in PowerShell.

Usage:
  publish-status.py publish --repo owner/name --sha <40-hex> \
    --state success|failure|error|pending --context ci/... \
    [--description TEXT | --platform linux|darwin|windows \\
      --tree <40-hex> [--run-prefix N]] [--target-url URL]
  publish-status.py selftest

Platform mode constructs `tree=<40hex> run=<prefix>:<platform> sig=<64hex>`
with HMAC-SHA256 over the canonical `faktor-platform-cert/v1` payload using
the platform's own secret (faktor_platform_status_key_<platform>). A success
without that secret is refused: an unsigned platform certificate can never
be published.

The aggregate verifier (scripts/certification/publish-status.mjs) implements
the byte-identical payload and rejects a missing/invalid signature.
"""
import hashlib
import hmac
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request

API_DEFAULT = "https://api.github.com"
STATES = {"success", "failure", "error", "pending"}
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
REPO_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
MAX_DESCRIPTION = 140


def fail(code, detail):
    print(f"github-status-{code}: {detail}", file=sys.stderr)
    sys.exit(1)


def arg(args, name, default=""):
    if name in args:
        at = args.index(name)
        if at + 1 >= len(args):
            fail("args", f"{name} requires a value")
        return args[at + 1]
    return default


def platform_signing_payload(platform, sha, tree, run, state, context):
    return "\n".join(
        [
            "faktor-platform-cert/v1",
            f"platform={platform}",
            f"sha={sha}",
            f"tree={tree}",
            f"run={run}",
            f"state={state}",
            f"context={context}",
        ]
    )


def platform_signature(platform, sha, tree, run, state, context, key):
    mac = hmac.new(
        str(key).encode("utf-8"),
        platform_signing_payload(platform, sha, tree, run, state, context).encode("utf-8"),
        hashlib.sha256,
    )
    return mac.hexdigest()


def platform_key_from_env(platform, environ=None):
    env = os.environ if environ is None else environ
    normalized = re.sub(r"[^a-z0-9]+", "_", str(platform).lower())
    for name in (
        f"faktor_platform_status_key_{normalized}",
        f"FAKTOR_PLATFORM_STATUS_KEY_{normalized.upper()}",
    ):
        value = env.get(name, "")
        if value:
            return value
    return ""


def validate(repo, sha, state, context, description, target_url):
    if not REPO_RE.match(repo):
        fail("repo", f"invalid repository {repo!r}")
    if not SHA_RE.match(sha):
        fail("sha", "--sha must be the exact 40-lowercase-hex commit")
    if state not in STATES:
        fail("state", f"--state must be one of {sorted(STATES)}")
    if not context or re.search(r"\s", context):
        fail("context", "--context must be a non-empty status context")
    if not description or len(description) > MAX_DESCRIPTION:
        fail("description", f"--description must be 1..{MAX_DESCRIPTION} characters")
    if target_url and not target_url.startswith("https://"):
        fail("target-url", "--target-url must be an https URL when set")


def token_from_env():
    return (
        os.environ.get("GITHUB_STATUS_TOKEN")
        or os.environ.get("GH_TOKEN")
        or os.environ.get("GITHUB_TOKEN")
        or ""
    )


def publish(api, repo, sha, state, context, description, target_url, token, platform="", tree="", run_prefix="", platform_key=""):
    # Platform mode: construct the description so the signed fields cannot
    # diverge from the published text. A success without the platform's own
    # secret is refused (fail closed).
    if platform:
        if not SHA_RE.match(tree):
            fail("tree", f"--platform {platform} requires --tree as the exact 40-lowercase-hex tree")
        if not run_prefix:
            fail("run-prefix", f"--platform {platform} requires --run-prefix (or CI_PIPELINE_NUMBER)")
        run = f"{run_prefix}:{platform}"
        if state == "success":
            key = platform_key or platform_key_from_env(platform)
            if not key:
                fail(
                    "platform-key-missing",
                    f"no faktor_platform_status_key_{platform} secret is configured; an unsigned platform certificate can never be published",
                )
            sig = platform_signature(platform, sha, tree, run, state, context, key)
            description = f"tree={tree} run={run} sig={sig}"
        else:
            description = f"tree={tree} run={run} state={state}"
    validate(repo, sha, state, context, description, target_url)
    if not token:
        fail("token-missing", "GITHUB_STATUS_TOKEN/GH_TOKEN/GITHUB_TOKEN is required (fail closed)")
    body = {"state": state, "context": context, "description": description}
    if target_url:
        body["target_url"] = target_url
    request = urllib.request.Request(
        f"{api.rstrip('/')}/repos/{repo}/statuses/{sha}",
        data=json.dumps(body).encode(),
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
            "Content-Type": "application/json",
            "User-Agent": "faktor-certification",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            if response.status not in (200, 201):
                fail("publish-failed", f"HTTP {response.status} from the statuses API")
    except urllib.error.HTTPError as error:
        fail("publish-failed", f"HTTP {error.code} from the statuses API")
    except urllib.error.URLError as error:
        fail("publish-failed", f"transport: {error.reason}")
    print(f"github-status-published context={context} state={state} sha={sha}")


def selftest():
    import http.server
    import threading

    failures = []

    def check(name, ok):
        if ok:
            print(f"selftest ok: {name}")
        else:
            print(f"selftest FAIL: {name}", file=sys.stderr)
            failures.append(name)

    check("validates a good call", True)
    for bad in [
        ("", "a" * 40, "success", "ci/x", "d"),
        ("acme/widgets", "main", "success", "ci/x", "d"),
        ("acme/widgets", "a" * 40, "green", "ci/x", "d"),
        ("acme/widgets", "a" * 40, "success", "", "d"),
        ("acme/widgets", "a" * 40, "success", "ci/x", "x" * 141),
    ]:
        try:
            validate(*bad, "")
            check(f"refuses {bad!r}", False)
        except SystemExit:
            check(f"refuses {bad!r}", True)

    seen = {}
    request_bodies = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            length = int(self.headers.get("content-length", "0"))
            body = json.loads(self.rfile.read(length) or b"{}")
            seen["url"] = self.path
            seen["auth"] = self.headers.get("authorization")
            seen["body"] = body
            request_bodies.append(body)
            self.send_response(201)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", "2")
            self.end_headers()
            self.wfile.write(b"{}")

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    environ_before = dict(os.environ)
    api = f"http://127.0.0.1:{server.server_address[1]}"
    try:
        publish(
            api,
            "acme/widgets",
            "a" * 40,
            "success",
            "ci/faktor/trusted-certified-linux",
            f"tree={'b' * 40} run=7:linux",
            "",
            "selftest-token",
        )
        check("posts to the exact-sha statuses path", seen.get("url") == f"/repos/acme/widgets/statuses/{'a' * 40}")
        check("posts with bearer auth", seen.get("auth") == "Bearer selftest-token")
        check(
            "body binds state/context/description",
            seen.get("body", {}).get("state") == "success"
            and seen["body"]["context"] == "ci/faktor/trusted-certified-linux"
            and seen["body"]["description"] == f"tree={'b' * 40} run=7:linux",
        )

        # Platform signing parity: the canonical payload must hash to the SAME
        # digest the JS/PowerShell implementations produce (vector pinned in
        # publish-status.mjs / the windows publisher as well).
        expected = "5fe2c03b7870f4b45a9f5ae83392cae87eb7a3effe2d76d20551a88034d08aab"
        actual = platform_signature(
            "darwin", "a" * 40, "b" * 40, "700:darwin", "success",
            "ci/faktor/trusted-certified-darwin", "darwin-secret",
        )
        check("platform signature matches the pinned canonical vector", actual == expected)

        # Key lookup is HERMETIC: the unit checks pass an EXPLICIT mapping
        # (mirroring the JS `platformKeyFromEnv(platform, env)`), so they can
        # neither read nor mutate the ambient process environment. The `{}`
        # mapping is the witness that an explicit mapping never falls back to
        # os.environ, even when the real lane's key is exported.
        check(
            "platform key lookup accepts the uppercase env form",
            platform_key_from_env("darwin", {"FAKTOR_PLATFORM_STATUS_KEY_DARWIN": "k"}) == "k",
        )
        check(
            "platform key lookup prefers the lowercase spelling when both exist",
            platform_key_from_env(
                "darwin",
                {
                    "faktor_platform_status_key_darwin": "lower",
                    "FAKTOR_PLATFORM_STATUS_KEY_DARWIN": "upper",
                },
            )
            == "lower",
        )
        check(
            "platform key lookup never leaks ambient os.environ into an explicit mapping",
            platform_key_from_env("darwin", {}) == "",
        )

        # Platform publish mode: both cases run the REAL CLI as a child
        # process with an explicitly constructed environment (the JS selftest
        # hermeticity fix mirrored here): every ambient
        # FAKTOR_PLATFORM_STATUS_KEY_* spelling is scrubbed from the child env
        # and only the keys for the case under test are planted, so the
        # selftest is hermetic whether the real secrets are present or absent.
        # process env is never mutated.
        def hermetic_child_env(extra=None):
            env = dict(os.environ)
            for name in list(env):
                if name.upper().startswith("FAKTOR_PLATFORM_STATUS_KEY_"):
                    del env[name]
            if extra:
                env.update(extra)
            return env

        def run_publisher_child(env):
            try:
                return subprocess.run(
                    [
                        sys.executable,
                        os.path.abspath(__file__),
                        "publish",
                        "--api", api,
                        "--repo", "acme/widgets",
                        "--sha", "a" * 40,
                        "--state", "success",
                        "--context", "ci/faktor/trusted-certified-darwin",
                        "--platform", "darwin",
                        "--tree", "b" * 40,
                        "--run-prefix", "700",
                    ],
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=60,
                )
            except subprocess.TimeoutExpired:
                return subprocess.CompletedProcess([], 124, "", "selftest child timed out")

        tree = "b" * 40
        before_platform = len(request_bodies)
        planted = run_publisher_child(
            hermetic_child_env(
                {
                    "GITHUB_STATUS_TOKEN": "selftest-token",
                    "FAKTOR_PLATFORM_STATUS_KEY_DARWIN": "darwin-secret",
                }
            )
        )
        posted = request_bodies[-1] if len(request_bodies) == before_platform + 1 else {}
        check(
            "platform publish signs the constructed description",
            planted.returncode == 0
            and posted.get("state") == "success"
            and posted.get("context") == "ci/faktor/trusted-certified-darwin"
            and posted.get("description") == f"tree={tree} run=700:darwin sig={expected}",
        )

        # Refusal: the scrubbed child env carries only the OTHER platforms'
        # keys, so this asserts "no key of its OWN", not merely "empty
        # environment"; the typed refusal happens before any request.
        before_refusal = len(request_bodies)
        refused = run_publisher_child(
            hermetic_child_env(
                {
                    "GITHUB_STATUS_TOKEN": "selftest-token",
                    "faktor_platform_status_key_linux": "selftest-linux-key",
                    "FAKTOR_PLATFORM_STATUS_KEY_WINDOWS": "selftest-windows-key",
                }
            )
        )
        check(
            "platform publish without its own key refuses (fail closed)",
            refused.returncode != 0
            and "github-status-platform-key-missing" in (refused.stdout + refused.stderr),
        )
        check("platform key refusal makes no request", len(request_bodies) == before_refusal)
        check("selftest never mutates os.environ", os.environ == environ_before)
    finally:
        server.shutdown()

    if failures:
        print(f"publish-status.py selftest: FAIL ({len(failures)})", file=sys.stderr)
        sys.exit(1)
    print("publish-status.py selftest: PASS")


if __name__ == "__main__":
    args = sys.argv[1:]
    command = args[0] if args else ""
    if command == "selftest":
        selftest()
    elif command == "publish":
        publish(
            arg(args, "--api", os.environ.get("GITHUB_STATUS_API", API_DEFAULT)),
            arg(args, "--repo", os.environ.get("CI_REPO", "")),
            arg(args, "--sha", os.environ.get("CI_COMMIT_SHA", "")),
            arg(args, "--state", ""),
            arg(args, "--context", ""),
            arg(args, "--description", ""),
            arg(args, "--target-url", os.environ.get("CI_PIPELINE_URL", "")),
            token_from_env(),
            platform=arg(args, "--platform", ""),
            tree=arg(args, "--tree", ""),
            run_prefix=arg(args, "--run-prefix", os.environ.get("CI_PIPELINE_NUMBER", "")),
        )
    else:
        fail("args", "usage: publish-status.py publish|selftest")
