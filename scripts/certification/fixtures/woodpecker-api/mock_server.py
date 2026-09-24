#!/usr/bin/env python3
"""Hermetic Woodpecker API mock for scripts/certify.sh --selftest.

Serves only the routes the certification gate queries:

    GET /api/repos/lookup/acme/widgets[?project=trusted]
    GET /api/repos/<id>                              (repo detail: trusted.volumes, config_file)
    GET /api/repos/<id>/pipelines[?query...]
    GET /api/repos/<id>/pipelines/<number>
    GET /api/repos/<id>/pipelines/<number>/logs/<step>

The response state is (re)read from `<state_dir>/state.json` on every
request, so the selftest can rewrite it between cases without restarting the
server. A request without `Authorization: Bearer selftest-token` gets 401.
This fixture exists to prove the gate's rejection matrix offline; it is not
a general Woodpecker emulator.
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlsplit


def main() -> int:
    port = int(sys.argv[1])
    state_dir = sys.argv[2]

    def load():
        with open(f"{state_dir}/state.json") as fh:
            return json.load(fh)

    def default_repo(repo_id):
        if repo_id == 7:
            return {
                "id": 7,
                "full_name": "acme/widgets",
                "config_file": ".woodpecker/untrusted/",
                "trusted": {"volumes": False},
            }
        if repo_id == 8:
            return {
                "id": 8,
                "full_name": "acme/widgets",
                "config_file": ".woodpecker/trusted/",
                "trusted": {"volumes": True},
            }
        return None

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, fmt, *args):  # keep the selftest output clean
            return

        def reply(self, status, payload):
            body = json.dumps(payload).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.headers.get("Authorization") != "Bearer selftest-token":
                self.reply(401, {"message": "unauthorized"})
                return
            parts = urlsplit(self.path)
            path = parts.path
            query = parse_qs(parts.query)
            state = load()
            if path == "/api/repos/lookup/acme/widgets":
                if query.get("project") == ["trusted"]:
                    self.reply(200, state.get("lookup_trusted") or default_repo(8))
                else:
                    self.reply(200, state.get("lookup") or default_repo(7))
                return
            if path.startswith("/api/repos/"):
                segments = path[len("/api/repos/") :].split("/")
                if len(segments) == 1 and segments[0].isdigit():
                    repo_id = int(segments[0])
                    repo = state.get(f"repo{repo_id}") or default_repo(repo_id)
                    if repo is None:
                        self.reply(404, {"message": "repo not found"})
                    else:
                        self.reply(200, repo)
                    return
                if len(segments) >= 2 and segments[1] == "pipelines":
                    if len(segments) == 2:
                        self.reply(200, state.get("pipelines") or [])
                        return
                    number = segments[2]
                    if len(segments) == 3:
                        details = state.get("details") or {}
                        detail = details.get(str(number)) or details.get(number)
                        if detail is None:
                            candidate = state.get("detail")
                            if isinstance(candidate, dict) and str(candidate.get("number")) == number:
                                detail = candidate
                        if not isinstance(detail, dict):
                            self.reply(404, {"message": "pipeline not found"})
                        else:
                            self.reply(200, detail)
                        return
                    if len(segments) == 5 and segments[3] == "logs":
                        step = segments[4]
                        logs = state.get("logs") or {}
                        if step in logs:
                            self.reply(200, logs[step])
                        else:
                            self.reply(404, {"message": "log not found"})
                        return
            self.reply(404, {"message": "not found"})

    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
