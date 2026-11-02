"""The import trial driver's `run.sh` refuses any folder that isn't new or empty, which is what
keeps it off the baseline (plan import-material-fixes). A fake binary gets it past its first check,
and a `python3` that exits at once stops it at the server, before port 1420 or a KWin session.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest

RUN = Path(__file__).resolve().parents[3] / "apps/desktop/scripts/import-trial/run.sh"
REFUSED = "A trial only runs in a new folder."
PAST_THE_CHECK = "The server couldn't start."
# A server that stays up, for a run that goes on to its first launch.
LASTING_SERVER = "#!/bin/sh\nexec sleep 30\n"


def executable(path: Path, text: str = "#!/bin/sh\nexit 0\n") -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(0o755)


def run_trial(
    tmp_path: Path,
    folder: Path,
    stage: str | None = None,
    ui: bool = False,
    server: str = "#!/bin/sh\nexit 0\n",
) -> subprocess.CompletedProcess:
    repo = tmp_path / "repo"
    executable(repo / "apps/desktop/src-tauri/target/debug/opendam-desktop")
    executable(tmp_path / "bin/python3", server)
    env = os.environ | {
        "TRIAL_REPO": str(repo),
        "PATH": f"{tmp_path / 'bin'}{os.pathsep}{os.environ['PATH']}",
    }
    env.pop("TRIAL_PACKS", None)
    env.pop("TRIAL_STAGE", None)
    if stage:
        env["TRIAL_STAGE"] = stage
    env.pop("TRIAL_UI", None)
    if ui:
        env["TRIAL_UI"] = "1"
    return subprocess.run([RUN, folder], env=env, capture_output=True, text=True, timeout=30)


@pytest.mark.parametrize("name", ["sentinel", ".hidden"])
def test_a_folder_with_anything_in_it_is_refused_and_left_alone(tmp_path, name):
    folder = tmp_path / "baseline"
    folder.mkdir()
    sentinel = folder / name
    sentinel.write_text("keep me")
    before = sentinel.stat().st_mtime_ns

    result = run_trial(tmp_path, folder)

    assert result.returncode == 1
    assert REFUSED in result.stderr
    assert PAST_THE_CHECK not in result.stderr
    assert [p.name for p in folder.iterdir()] == [name]
    assert sentinel.read_text() == "keep me"
    assert sentinel.stat().st_mtime_ns == before


@pytest.mark.parametrize("exists", [False, True])
def test_a_new_or_empty_folder_gets_past_the_check(tmp_path, exists):
    folder = tmp_path / "trial"
    if exists:
        folder.mkdir()

    result = run_trial(tmp_path, folder)

    assert result.returncode == 1
    assert REFUSED not in result.stderr
    assert PAST_THE_CHECK in result.stderr
    assert (folder / "h").is_dir()


def test_a_stage_opens_no_folder_the_script_did_not_make(tmp_path):
    folder = tmp_path / "baseline"
    (folder / "h").mkdir(parents=True)
    sentinel = folder / "sentinel"
    sentinel.write_text("keep me")

    result = run_trial(tmp_path, folder, stage="kenney")

    assert result.returncode == 1
    assert REFUSED in result.stderr
    assert PAST_THE_CHECK not in result.stderr
    assert not (folder / "h/trial").exists()


def test_a_profile_of_its_own_opens_under_a_new_stage(tmp_path):
    folder = tmp_path / "trial"
    (folder / "h").mkdir(parents=True)
    (folder / "h/trial").touch()

    assert PAST_THE_CHECK in run_trial(tmp_path, folder, stage="kenney").stderr


def test_a_profile_never_runs_a_stage_twice(tmp_path):
    folder = tmp_path / "trial"
    (folder / "h").mkdir(parents=True)
    (folder / "h/trial").touch()
    (folder / "h/log-kenney.jsonl").write_text("{}\n")

    result = run_trial(tmp_path, folder, stage="kenney")

    assert result.returncode == 1
    assert "already ran stage kenney" in result.stderr
    assert PAST_THE_CHECK not in result.stderr


def test_a_stage_never_deletes_the_log_of_an_earlier_run(tmp_path):
    folder = tmp_path / "trial"
    (folder / "h").mkdir(parents=True)
    (folder / "h/trial").touch()
    log = folder / "h/log.jsonl"
    log.write_text("the first run\n")

    result = run_trial(tmp_path, folder, stage="kenney")

    assert result.returncode == 1
    assert "still holds h/log.jsonl" in result.stderr
    assert PAST_THE_CHECK not in result.stderr
    assert log.read_text() == "the first run\n"


def test_the_default_stage_never_reopens_a_profile(tmp_path):
    folder = tmp_path / "trial"
    (folder / "h").mkdir(parents=True)
    (folder / "h/trial").touch()

    result = run_trial(tmp_path, folder)

    assert result.returncode == 1
    assert REFUSED in result.stderr


def test_a_missing_frontend_stops_before_the_folder_is_touched(tmp_path):
    folder = tmp_path / "trial"

    result = run_trial(tmp_path, folder, ui=True)

    assert result.returncode == 1
    assert "No built frontend" in result.stderr
    assert not folder.exists()


def test_a_profile_whose_first_launch_failed_is_refused_not_reopened(tmp_path):
    folder = tmp_path / "trial"
    executable(tmp_path / "bin/dbus-run-session")

    first = run_trial(tmp_path, folder, stage="packs", server=LASTING_SERVER)

    assert first.returncode == 1
    assert "made no master DB" in first.stderr
    assert not (folder / "h/trial").exists()

    second = run_trial(tmp_path, folder, stage="packs")

    assert second.returncode == 1
    assert REFUSED in second.stderr


def test_a_set_up_profile_is_marked_as_one(tmp_path):
    folder = tmp_path / "trial"
    master = folder / "data/com.opendam.desktop"
    executable(
        tmp_path / "bin/dbus-run-session",
        f'#!/bin/sh\nmkdir -p "{master}"\n: > "{master}/opendam.sqlite"\n',
    )
    executable(tmp_path / "bin/sqlite3")

    run_trial(tmp_path, folder, stage="packs", server=LASTING_SERVER)

    assert (folder / "h/trial").exists()
