"""Serves the import trial's page on the port of the app's devUrl and keeps the log it posts.

    python3 -I server.py <trial folder>

The page comes from beside this file, the pack list from the trial's h/packs.json with each `~/`
path expanded, and every POST is appended to h/log.jsonl.
"""

import http.server
import json
import os
import sys
from pathlib import Path

PORT = 1420  # `build.devUrl` in tauri.conf.json
PAGE = Path(__file__).resolve().parent / "index.html"
trial = Path(sys.argv[1]) / "h"


def pack_list() -> bytes:
    packs = json.loads((trial / "packs.json").read_text(encoding="utf-8"))
    for pack in packs["imports"]:
        pack["path"] = os.path.expanduser(pack["path"])
    return json.dumps(packs).encode()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/":
            body, kind = PAGE.read_bytes(), "text/html"
        elif path == "/packs.json":
            body, kind = pack_list(), "application/json"
        else:
            self.send_response(404)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        with (trial / "log.jsonl").open("ab") as log:
            log.write(body + b"\n")
        self.send_response(204)
        self.end_headers()


http.server.ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
