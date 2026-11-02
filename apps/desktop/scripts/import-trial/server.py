"""Serves the import trial's page on the port of the app's devUrl and keeps the log it posts.

    python3 -I server.py <trial folder> [<built frontend>]

The page comes from beside this file, the pack list from the trial's h/packs.json with each `~/`
path expanded, and every POST is appended to h/log.jsonl. Given a built frontend, it serves that
instead, with gate.js loaded first and the bundle's one IPC call site sent through it.
"""

import http.server
import json
import mimetypes
import os
import sys
from pathlib import Path

PORT = 1420  # `build.devUrl` in tauri.conf.json
HERE = Path(__file__).resolve().parent
PAGE = HERE / "index.html"
IPC_CALL = b"window.__TAURI_INTERNALS__.invoke("
GATE_CALL = b"window.__gateInvoke("
trial = Path(sys.argv[1]) / "h"
frontend = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else None


def pack_list() -> bytes:
    packs = json.loads((trial / "packs.json").read_text(encoding="utf-8"))
    for pack in packs["imports"]:
        pack["path"] = os.path.expanduser(pack["path"])
    return json.dumps(packs).encode()


def frontend_page() -> bytes:
    page = (frontend / "index.html").read_bytes()
    return page.replace(b"<script type", b'<script src="/gate.js"></script><script type', 1)


def frontend_file(path: str) -> tuple[bytes, str] | None:
    """A file of the built frontend, with the IPC call site rerouted, or None if it has no such."""
    file = (frontend / path.lstrip("/")).resolve()
    if frontend not in file.parents or not file.is_file():
        return None
    body = file.read_bytes()
    if file.suffix == ".js":
        return body.replace(IPC_CALL, GATE_CALL), "text/javascript"
    return body, mimetypes.guess_type(file.name)[0] or "application/octet-stream"


def page(path: str) -> tuple[bytes, str] | None:
    if path == "/packs.json":
        return pack_list(), "application/json"
    if frontend is None:
        return (PAGE.read_bytes(), "text/html") if path == "/" else None
    if path == "/":
        return frontend_page(), "text/html"
    if path == "/gate.js":
        return (HERE / "gate.js").read_bytes(), "text/javascript"
    return frontend_file(path)


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass

    def do_GET(self):
        found = page(self.path.split("?")[0])
        if found is None:
            self.send_response(404)
            self.end_headers()
            return
        body, kind = found
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
