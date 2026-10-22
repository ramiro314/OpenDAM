"""The measurement script's worker client (plan textured-fbx-previews C12).

A stub worker answers `engine_info`, then misbehaves on `model_meta`, so a timeout and a worker
that dies are reported apart: a death is a finding, never a timeout.
"""

from __future__ import annotations

import importlib.util
import sqlite3
import sys
from pathlib import Path

import pytest

TOOL = Path(__file__).resolve().parent.parent / "tools" / "measure_textures.py"
spec = importlib.util.spec_from_file_location("measure_textures", TOOL)
measure_textures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(measure_textures)

STUB = """
import json, os, signal, sys, time
for line in sys.stdin:
    req = json.loads(line)
    # What to do: `do`, else the stem of `source` as `measure` sends it
    do = req["params"].get("do") or os.path.basename(req["params"].get("source", "")).split(".")[0]
    if req["method"] == "engine_info":
        print(json.dumps({"id": req["id"], "result": {"preview_version": 2}}), flush=True)
    elif do == "crash":
        os.kill(os.getpid(), signal.SIGSEGV)
    elif do == "exit":
        sys.exit(3)
    elif do == "hang":
        time.sleep(30)
    elif do == "empty":
        print(json.dumps({"id": req["id"], "result": {"ok": True}}), flush=True)
    else:
        print(json.dumps({"id": "stray"}), flush=True)
        print(json.dumps({"id": req["id"], "result": {"ok": True}}), flush=True)
"""


@pytest.fixture
def worker(tmp_path):
    stub = tmp_path / "stub.py"
    stub.write_text(STUB)
    with open(tmp_path / "worker.log", "w") as log:
        w = measure_textures.Worker(log, [sys.executable, str(stub)])
        yield w
        w.close()


def test_a_reply_is_matched_by_id_with_its_peak_rss(worker):
    reply, seconds, peak, failure = worker.call("model_meta", {}, timeout=10)
    assert reply["result"] == {"ok": True}
    assert failure is None
    assert peak > 0
    assert seconds < 10


@pytest.mark.parametrize(
    ("do", "failure"),
    [("crash", "worker died (signal 11)"), ("exit", "worker died (exit status 3)")],
)
def test_a_worker_that_dies_is_no_timeout_and_its_peak_is_unknown(worker, do, failure):
    pid = worker.proc.pid
    reply, seconds, peak, got = worker.call("model_meta", {"do": do}, timeout=10)
    assert (reply, peak, got) == (None, None, failure)
    assert seconds < 5
    # A new worker serves the next request
    assert worker.proc.pid != pid
    assert worker.call("model_meta", {}, timeout=10)[0]["result"] == {"ok": True}


def test_a_request_past_its_deadline_is_a_timeout(worker):
    pid = worker.proc.pid
    reply, seconds, peak, failure = worker.call("model_meta", {"do": "hang"}, timeout=0.5)
    assert reply is None
    assert failure == "timeout after 0.5 s"
    assert seconds >= 0.5
    assert peak > 0
    assert worker.proc.pid != pid


def test_a_pack_row_counts_deaths_and_timeouts_as_themselves(worker, tmp_path, monkeypatch):
    monkeypatch.setattr(measure_textures, "JOB_TIMEOUT", 0.5)
    conn = sqlite3.connect(":memory:")
    conn.execute("CREATE TABLE folders (path TEXT, is_pack INTEGER)")
    rows = [
        {"id": stem, "file_path": f"Kit/{stem}.fbx", "size_bytes": 1, "app_previews": 1}
        for stem in ("crash", "hang", "empty")
    ]
    results = [measure_textures.measure(worker, conn, tmp_path, tmp_path / "out", r) for r in rows]

    assert [r["error"] for r in results] == [
        "worker died (signal 11)",
        "timeout after 0.5 s",
        "no preview",
    ]
    summary = measure_textures.summarize(results)
    assert summary["loaded"] == 0
    assert summary["failed"] == {
        "worker died (signal 11)": 1,
        "timeout after 0.5 s": 1,
        "no preview": 1,
    }


def test_app_previews_follows_the_queue_claim_rule():
    conn = sqlite3.connect(":memory:")
    conn.row_factory = sqlite3.Row
    conn.execute(
        """CREATE TABLE assets (id TEXT, file_path TEXT, extension TEXT, size_bytes INTEGER,
           group_id TEXT, support_role TEXT, bundled_preview TEXT, render_width INTEGER,
           render_height INTEGER, preview_status TEXT, thumbnail_path TEXT)"""
    )
    rows = [
        # id, path, group, support, bundled preview, size of a render, preview status, thumb
        ("single", "single.fbx", None, None, None, None, "done", "t.png"),
        ("glb", "glb.glb", "glb", None, None, None, "done", "t.png"),
        ("member", "member.fbx", "glb", None, None, None, "done", "t.png"),
        ("blend", "blend.blend", "blend", None, None, None, "none", None),
        ("stand_in", "stand_in.fbx", "blend", None, None, None, "done", "t.png"),
        ("support", "support.fbx", None, "model_preview", None, None, "done", "t.png"),
        ("pictured", "pictured.fbx", None, None, "big.png", None, "done", "t.png"),
        ("small", "small.fbx", None, None, "tiny.png", None, "done", "t.png"),
        ("big", "big.png", None, None, None, 1024, "done", None),
        ("tiny", "tiny.png", None, None, None, 64, "done", None),
    ]
    for i, path, group, support, bundled, side, status, thumb in rows:
        ext = path.rsplit(".", 1)[1]
        conn.execute(
            "INSERT INTO assets VALUES (?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?)",
            (i, path, ext, group, support, bundled, side, side, status, thumb),
        )

    got = {r["id"]: r["app_previews"] for r in measure_textures.fbx_rows(conn, "1", ())}
    assert got == {
        "single": 1,
        "member": 0,  # its GLB main item is what the grid shows
        "stand_in": 1,  # stands in for a Blend main item with no preview
        "support": 0,
        "pictured": 0,  # its render fits a card
        "small": 1,  # its render is too small to picture it
    }
