"""Reads a gate profile's stage logs and library DBs into one row per pack, read-only.

    python3 -I gate_check.py <trial folder> <stage> [--tsv <out>]

Each `tree` line of h/log-<stage>.jsonl gives a ticked pack's target and the dialog's tree. A row
adds that pack's latest run, its manifest and the files in its folder, and fails when the run
isn't `done` with no error, the pack isn't built, or its files don't match: the tree's count, in
which a conversion's GLBs are apart from its files (an Unreal tree's is only an estimate, so there
its GLBs must be above 0 and at most the estimate), the manifest's and the folder's. A run's
warning fails its row too, as it can name a model that failed. The exit status is 1 when a row
fails, or when the log shows a dialog that didn't start.
"""

import argparse
import csv
import json
import sqlite3
import sys
from collections import Counter
from pathlib import Path

COLUMNS = [
    "pack",
    "run",
    "seconds",
    "warnings",
    "files_tree",
    "files_manifest",
    "files_folder",
    "glbs_tree",
    "glbs_folder",
    "skipped",
    "verdict",
]


def read_log(path: Path) -> list[dict]:
    with path.open(encoding="utf-8") as log:
        return [json.loads(line) for line in log]


def library_dbs(folder: Path) -> dict[str, sqlite3.Connection]:
    dbs = {}
    for db in sorted((folder / "libs").glob("*/.opendam/library.sqlite")):
        conn = sqlite3.connect(f"{db.resolve().as_uri()}?mode=ro", uri=True)
        conn.row_factory = sqlite3.Row
        dbs[db.parent.parent.name] = conn
    return dbs


def find_pack(dbs: dict[str, sqlite3.Connection], candidate: dict):
    """The library root's name, its connection and the pack's row, or None. The log holds the
    scan's target, from before the dialog's publisher replaced the publisher in front of it, so a
    target that names no pack falls back to the pack's name under any publisher."""
    for query, value in (
        ("target = ?", candidate["target"]),
        ("target LIKE '%/' || ?", candidate["name"]),
    ):
        for name, conn in dbs.items():
            row = conn.execute(f"SELECT * FROM import_packs WHERE {query}", (value,)).fetchone()
            if row is not None:
                return name, conn, row
    return None


def latest_run(conn: sqlite3.Connection, pack_id: str):
    runs = conn.execute("SELECT * FROM import_runs ORDER BY created_at DESC").fetchall()
    for run in runs:
        if any(pack["packId"] == pack_id for pack in json.loads(run["packs"])):
            return run
    return None


def folder_files(folder: Path) -> list[Path]:
    return [path for path in folder.rglob("*") if path.is_file()]


def check(candidate: dict, found, libs: Path) -> dict:
    row = dict.fromkeys(COLUMNS, "")
    row["pack"] = candidate["target"]
    row["files_tree"] = candidate["files"]
    row["glbs_tree"] = candidate["glbs"]
    row["skipped"] = len(candidate["skipped"])
    if found is None:
        row["verdict"] = "FAIL: no pack row"
        return row
    library, conn, pack = found
    row["pack"] = pack["target"]
    run = latest_run(conn, pack["id"])
    manifest = conn.execute(
        "SELECT count(*) FROM import_outputs WHERE pack_id = ? AND build_id = ?",
        (pack["id"], pack["current_build"]),
    ).fetchone()[0]
    files = folder_files(libs / library / pack["target"])
    glbs = sum(path.suffix.lower() == ".glb" for path in files)
    row["files_manifest"] = manifest
    row["files_folder"] = len(files)
    row["glbs_folder"] = glbs
    problems = []
    if run is None:
        problems.append("no run")
    else:
        row["run"] = run["status"]
        row["seconds"] = round((run["finished_at"] - run["started_at"]) / 1000)
        warnings = json.loads(run["warnings"])
        row["warnings"] = len(warnings)
        if warnings:
            problems.append(f"{len(warnings)} run warnings, such as a failed model")
        if run["status"] != "done" or run["error"]:
            problems.append(f"run {run['status']}: {run['error']}")
    if pack["status"] != "built":
        problems.append(f"pack {pack['status']}")
    if manifest != len(files):
        problems.append("manifest and folder differ")
    if candidate["estimate"]:
        if not 0 < glbs <= candidate["glbs"]:
            problems.append("GLBs outside 0 and the estimate")
    elif candidate["files"] + candidate["glbs"] != len(files):
        problems.append("tree and folder differ")
    row["verdict"] = "; ".join(problems) or "pass"
    return row


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("folder", type=Path)
    parser.add_argument("stage")
    parser.add_argument("--tsv", type=Path)
    args = parser.parse_args()

    log = read_log(args.folder / "h" / f"log-{args.stage}.jsonl")
    dbs = library_dbs(args.folder)
    rows = []
    skipped = []
    for line in log:
        if line["k"] != "tree":
            continue
        for candidate in line["v"]["candidates"]:
            if candidate["ticked"]:
                found = find_pack(dbs, candidate)
                rows.append(check(candidate, found, args.folder / "libs"))
                skipped += [(candidate["target"], entry) for entry in candidate["skipped"]]
    for conn in dbs.values():
        conn.close()

    out = args.tsv.open("w", newline="") if args.tsv else sys.stdout
    writer = csv.DictWriter(out, COLUMNS, delimiter="\t", lineterminator="\n")
    writer.writeheader()
    writer.writerows(rows)
    if args.tsv:
        out.close()

    print(f"\n{len(rows)} packs, {sum(r['verdict'] == 'pass' for r in rows)} pass", file=sys.stderr)
    for row in rows:
        if row["verdict"] != "pass":
            print(f"{row['pack']}: {row['verdict']}", file=sys.stderr)
    errors = [line["v"] for line in log if line["k"] == "start_err"]
    for error in errors:
        print(f"didn't start: {error}", file=sys.stderr)
    print(f"{len(skipped)} skipped entries", file=sys.stderr)
    reasons = Counter(str(entry.get("reason")) for _, entry in skipped)
    for reason, count in reasons.most_common():
        print(f"  {count} {reason}", file=sys.stderr)
    bad = errors or any(row["verdict"] != "pass" for row in rows)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
