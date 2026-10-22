"""The measurement script's worker client (plan textured-fbx-previews C12).

A stub worker answers `engine_info`, then misbehaves on `model_meta`, so a timeout and a worker
that dies are reported apart: a death is a finding, never a timeout.
"""

from __future__ import annotations

import importlib.util
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
    if req["method"] == "engine_info":
        print(json.dumps({"id": req["id"], "result": {"preview_version": 2}}), flush=True)
    elif req["params"].get("do") == "crash":
        os.kill(os.getpid(), signal.SIGSEGV)
    elif req["params"].get("do") == "exit":
        sys.exit(3)
    elif req["params"].get("do") == "hang":
        time.sleep(30)
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
