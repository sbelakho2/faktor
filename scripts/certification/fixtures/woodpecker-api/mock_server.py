#!/usr/bin/env python3
"""Hermetic Woodpecker API mock for scripts/certify.sh --selftest.

Serves only the routes the certification gate queries:

    GET /api/repos/lookup/acme/widgets
    GET /api/repos/7/pipelines[?query...]
    GET /api/repos/7/pipelines/<number>

The response state is (re)read from `<state_dir>/state.json` on every
request, so the selftest can rewrite it between cases without restarting the
server. A request without `Authorization: Bearer selftest-token` gets 401.
This fixture exists to prove the gate's rejection matrix offline; it is not
a general Woodpecker emulator.
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlsplit


def main() -> int:
    port = int(sys.argv[1])
    state_dir = sys.argv[2]

    def load():
        with open(f"{state_dir}/state.json") as fh:
            return json.load(fh)

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
            path = urlsplit(self.path).path
            state = load()
            if path == "/api/repos/lookup/acme/widgets":
                self.reply(200, {"id": 7, "full_name": "acme/widgets"})
            elif path == "/api/repos/7/pipelines":
                self.reply(200, state["pipelines"])
            elif path.startswith("/api/repos/7/pipelines/"):
                number = path.rsplit("/", 1)[1]
                detail = state["detail"]
                if str(detail.get("number")) != number:
                    self.reply(404, {"message": "pipeline not found"})
                else:
                    self.reply(200, detail)
            else:
                self.reply(404, {"message": "not found"})

    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
