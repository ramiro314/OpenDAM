"""The gate's read-only checks fail when they can't look, as well as when they find something: a
stage cut short by the memory guard, or a journal that doesn't reach its window, must never read
as a clean stage (plan import-material-fixes, "Before deleting the old libraries").
"""

from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import sys
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[3] / "apps/desktop/scripts/import-trial"
CHECK = SCRIPTS / "gate_check.py"
CAPS = SCRIPTS / "gate-caps.sh"
PACK = "Pub/Pack"
RUN_START = 1_000_000_000_000
SCOPE = "opendam-capped-111-222"


def write_log(folder: Path, stage: str, lines: list[dict]) -> None:
    h = folder / "h"
    h.mkdir(parents=True, exist_ok=True)
    text = "".join(json.dumps(line, separators=(",", ":")) + "\n" for line in lines)
    (h / f"log-{stage}.jsonl").write_text(text)


def tree_line(files: int = 1) -> dict:
    candidate = {
        "name": "Pack",
        "target": PACK,
        "ticked": True,
        "files": files,
        "glbs": 0,
        "estimate": False,
        "skipped": [],
    }
    return {"t": RUN_START, "k": "tree", "v": {"path": "a.zip", "candidates": [candidate]}}


def library(folder: Path, status: str = "done", started: int | None = 1, ended: int | None = 2):
    db = folder / "libs/Fab/.opendam/library.sqlite"
    db.parent.mkdir(parents=True)
    pack_folder = folder / "libs/Fab" / PACK
    pack_folder.mkdir(parents=True)
    (pack_folder / "a.png").write_text("x")
    with sqlite3.connect(db) as conn:
        conn.executescript(
            """
            CREATE TABLE import_packs (id, target, status, current_build);
            CREATE TABLE import_outputs (pack_id, build_id, path);
            CREATE TABLE import_runs (
                packs, status, error, warnings, created_at, started_at, finished_at
            );
            INSERT INTO import_packs VALUES ('p', 'Pub/Pack', 'built', 'b');
            INSERT INTO import_outputs VALUES ('p', 'b', 'a.png');
            """
        )
        conn.execute(
            "INSERT INTO import_runs VALUES (?, ?, NULL, '[]', 1, ?, ?)",
            ('[{"packId":"p"}]', status, started, ended),
        )
    conn.close()


def check(folder: Path, stage: str = "s") -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-I", CHECK, folder, stage], capture_output=True, text=True, timeout=30
    )


def test_a_finished_stage_passes(tmp_path):
    library(tmp_path)
    write_log(tmp_path, "s", [tree_line(), {"t": RUN_START, "k": "done", "v": True}])

    result = check(tmp_path)

    assert result.returncode == 0, result.stderr
    assert "1 packs, 1 pass" in result.stderr


def test_a_stage_with_no_pack_fails(tmp_path):
    library(tmp_path)
    write_log(tmp_path, "s", [{"t": RUN_START, "k": "done", "v": "Error: no scan"}])

    result = check(tmp_path)

    assert result.returncode == 1
    assert "names no pack" in result.stderr


def test_a_stage_cut_short_fails_though_its_packs_pass(tmp_path):
    library(tmp_path)
    write_log(tmp_path, "s", [tree_line()])

    result = check(tmp_path)

    assert result.returncode == 1
    assert "no true `done` line" in result.stderr


def test_an_unfinished_run_fails_its_row_and_prints_the_stage(tmp_path):
    library(tmp_path, status="running", started=1, ended=None)
    write_log(tmp_path, "s", [tree_line(), {"t": RUN_START, "k": "done", "v": True}])

    result = check(tmp_path)

    assert result.returncode == 1
    assert "Traceback" not in result.stderr
    assert "run running" in result.stderr
    assert PACK in result.stdout


def fake_journal(tmp_path: Path, oldest: int, user: str = "", kernel: str = "") -> dict:
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (tmp_path / "user.txt").write_text(user)
    (tmp_path / "kernel.txt").write_text(kernel)
    journalctl = bin_dir / "journalctl"
    journalctl.write_text(
        f"""#!/bin/sh
case "$*" in
  *short-unix*) echo "{oldest}.000000 host first entry" ;;
  *--user*) cat "{tmp_path}/user.txt" ;;
  *) cat "{tmp_path}/kernel.txt" ;;
esac
"""
    )
    journalctl.chmod(0o755)
    return os.environ | {"PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}"}


def stage_folder(tmp_path: Path) -> Path:
    folder = tmp_path / "trial"
    write_log(
        folder,
        "s",
        [
            {"t": RUN_START, "k": "start", "v": 1},
            {"t": RUN_START + 60_000, "k": "done", "v": True},
        ],
    )
    (folder / "h/app-s.log").write_text("all well\n")
    (folder / "h/watch.log").write_text(f"10:00:00 stage=s available=9000M {SCOPE}=9M/10240\n")
    return folder


def caps(folder: Path, env: dict) -> subprocess.CompletedProcess:
    return subprocess.run([CAPS, folder], env=env, capture_output=True, text=True, timeout=30)


CONSUMED = f"{SCOPE}.scope: Consumed 1s CPU time over 2s wall clock time, 1G memory peak.\n"
OLD_ENOUGH = RUN_START // 1000 - 3600


def test_a_clean_stage_passes_on_its_own_window(tmp_path):
    env = fake_journal(tmp_path, OLD_ENOUGH, user=CONSUMED)

    result = caps(stage_folder(tmp_path), env)

    assert result.returncode == 0, result.stdout
    assert "FAIL" not in result.stdout
    assert f"{SCOPE} peak 1G" in result.stdout


def test_a_journal_that_doesnt_reach_the_window_fails(tmp_path):
    env = fake_journal(tmp_path, RUN_START // 1000 + 3600, user=CONSUMED)

    result = caps(stage_folder(tmp_path), env)

    assert result.returncode == 1
    assert "doesn't reach back" in result.stdout


def test_a_log_with_no_timestamps_fails(tmp_path):
    folder = stage_folder(tmp_path)
    (folder / "h/log-s.jsonl").write_text("not json\n")

    result = caps(folder, fake_journal(tmp_path, OLD_ENOUGH, user=CONSUMED))

    assert result.returncode == 1
    assert "no window" in result.stdout


def test_a_kernel_oom_fails(tmp_path):
    env = fake_journal(
        tmp_path, OLD_ENOUGH, user=CONSUMED, kernel="Out of memory: Killed process 1\n"
    )

    assert caps(stage_folder(tmp_path), env).returncode == 1


def test_a_scope_oom_fails_and_a_clean_up_doesnt(tmp_path):
    cleanup = f"{SCOPE}.scope: Killed unit cgroup with SIGKILL on client request.\n"
    env = fake_journal(tmp_path, OLD_ENOUGH, user=CONSUMED + cleanup)
    assert caps(stage_folder(tmp_path), env).returncode == 0

    other = tmp_path / "other"
    other.mkdir()
    oom = f"{SCOPE}.scope: Failed with result 'oom-kill'.\n"
    env = fake_journal(other, OLD_ENOUGH, user=CONSUMED + oom)
    assert caps(stage_folder(tmp_path), env).returncode == 1


def test_a_cap_kill_in_the_app_log_fails(tmp_path):
    folder = stage_folder(tmp_path)
    (folder / "h/app-s.log").write_text("Exporting failed: killed by the memory cap (20G)\n")

    assert caps(folder, fake_journal(tmp_path, OLD_ENOUGH, user=CONSUMED)).returncode == 1


def test_a_stage_with_no_scopes_fails(tmp_path):
    folder = stage_folder(tmp_path)
    (folder / "h/watch.log").write_text("10:00:00 stage=s available=9000M\n")

    result = caps(folder, fake_journal(tmp_path, OLD_ENOUGH, user=CONSUMED))

    assert result.returncode == 1
    assert "names no scope" in result.stdout
