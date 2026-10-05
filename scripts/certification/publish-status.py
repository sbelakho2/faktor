#!/usr/bin/env python3
"""Stdlib GitHub commit-status publisher for platform lanes without node.

Parity port of the `publish` half of scripts/certification/publish-status.mjs
(validation + fail-closed POST). The darwin and windows certificate steps
publish their per-platform statuses through this script; the linux
certificate and the semantic aggregate use the node script.

Usage:
  publish-status.py publish --repo owner/name --sha <40-hex> \
    --state success|failure|error|pending --context ci/... \
    --description "tree=<40hex> run=<pipeline>:<platform>" [--target-url URL]
  publish-status.py selftest
"""
import json
import os
import re
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


def publish(api, repo, sha, state, context, description, target_url, token):
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

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            length = int(self.headers.get("content-length", "0"))
            seen["url"] = self.path
            seen["auth"] = self.headers.get("authorization")
            seen["body"] = json.loads(self.rfile.read(length) or b"{}")
            self.send_response(201)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", "2")
            self.end_headers()
            self.wfile.write(b"{}")

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        publish(
            f"http://127.0.0.1:{server.server_address[1]}",
            "acme/widgets",
            "a" * 40,
            "success",
            "ci/faktor/trusted-certified-linux",
            f"tree={'b' * 40} run=7:linux",
            "",
            "selftest-token",
        )
    finally:
        server.shutdown()
    check("posts to the exact-sha statuses path", seen.get("url") == f"/repos/acme/widgets/statuses/{'a' * 40}")
    check("posts with bearer auth", seen.get("auth") == "Bearer selftest-token")
    check(
        "body binds state/context/description",
        seen.get("body", {}).get("state") == "success"
        and seen["body"]["context"] == "ci/faktor/trusted-certified-linux"
        and seen["body"]["description"] == f"tree={'b' * 40} run=7:linux",
    )

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
        )
    else:
        fail("args", "usage: publish-status.py publish|selftest")
