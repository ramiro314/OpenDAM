"""
Measures textured FBX previews on a real library without writing to it (plan
textured-fbx-previews C12).

It samples FBX files in the given pack folders, plus the library's largest FBX, and runs
`model_meta` on each through one persistent `worker.py` process, with the same `library_root`
and `pack_root` the app sends. The library DB is opened read-only and every GLB goes to `--out`,
which must lie outside the library. It prints Markdown tables: one row per pack, the alpha
textures the sampled models used, and the large files against the app's 60 s job timeout.

Run it from the repository root under the memory cap (AGENTS.md, "Memory-heavy tools"):

    scripts/memory-cap.sh packages/preview-engine/.venv/bin/python \\
      packages/preview-engine/tools/measure_textures.py ~/Documents/Assets/Kenney \\
      --pack "3D assets/Car Kit" --pack "3D assets/Racing Kit" --out "$SCRATCH/kenney"
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import random
import sqlite3
import statistics
import subprocess
import sys
import threading
import time
from collections import Counter
from pathlib import Path

ENGINE = Path(__file__).resolve().parent.parent
JOB_TIMEOUT = 60  # seconds, `run_python_queue`'s timeout for one job

# The innermost folder marked as a pack holding a file's folder, as `preview::pack_root` reads it.
PACK_ROOT = """
SELECT path FROM folders
WHERE is_pack AND (path = '' OR path = ?1 OR substr(?1, 1, length(path) + 1) = path || '/')
ORDER BY length(path) DESC LIMIT 1
"""


def _pictured(item: str) -> str:
    """`commands::asset::pictured`: `item`'s bundled render fits a card (`THUMB_SIZE` 512)."""
    return f"""EXISTS (SELECT 1 FROM assets r WHERE r.file_path = {item}.bundled_preview
                AND r.render_width >= 512 AND r.render_height >= 512)"""


# Whether the app's queue would claim the file's preview job, as the model preview pass requeues
# it: not a support file (`NOT_SUPPORT`) and `preview::claimable`, so a main item or a file on
# its own that isn't pictured, or a member standing in for a main item with no preview
# (`preview::stands_in`). Other FBX are format-group members whose main item, often a GLB, is
# what the grid shows.
APP_PREVIEWS = f"""
a.support_role IS NULL AND (
  ((a.group_id IS NULL OR a.group_id = a.id) AND NOT {_pictured("a")})
  OR EXISTS (SELECT 1 FROM assets g
             WHERE g.id = a.group_id AND g.preview_status = 'none' AND g.thumbnail_path IS NULL
               AND NOT {_pictured("g")})
)
"""


class Worker:
    """One persistent `worker.py` process, asked one JSON request at a time, as the app does."""

    def __init__(self, log, command: list[str] | None = None):
        self.log = log
        self.command = command or [sys.executable, str(ENGINE / "worker.py")]
        self.next_id = 0
        self._start()

    def _start(self) -> None:
        self.proc = subprocess.Popen(
            self.command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.log,
            text=True,
            cwd=ENGINE,
        )
        self.lines: queue.Queue[str | None] = queue.Queue()
        threading.Thread(target=self._read, args=(self.proc, self.lines), daemon=True).start()
        reply = self.call("engine_info", {}, timeout=30)[0] or {}
        print(f"worker pid {self.proc.pid}, engine {reply.get('result')}", file=sys.stderr)

    @staticmethod
    def _read(proc, lines) -> None:
        for line in proc.stdout:
            lines.put(line)
        lines.put(None)

    def call(
        self, method: str, params: dict, timeout: float
    ) -> tuple[dict | None, float, int | None, str | None]:
        """
        `(reply, seconds, peak RSS in bytes, failure)`. After a timeout or the worker's death the
        reply is None, `failure` says which, and the worker is replaced. A dead worker's peak RSS
        is unknown (None): its `/proc` entry no longer holds one.
        """
        self.next_id += 1
        req_id = f"m{self.next_id}"
        _reset_peak(self.proc.pid)
        kills = _oom_kills()
        start = time.monotonic()
        self.proc.stdin.write(json.dumps({"id": req_id, "method": method, "params": params}))
        self.proc.stdin.write("\n")
        self.proc.stdin.flush()
        deadline = start + timeout
        while True:
            try:
                line = self.lines.get(timeout=max(0.0, deadline - time.monotonic()))
            except queue.Empty:
                # As the app does after a job timeout: kill the process and start another.
                elapsed, peak = time.monotonic() - start, _peak(self.proc.pid)
                self.proc.kill()
                self.proc.wait()
                return self._replace(method, None, elapsed, peak, f"timeout after {timeout:g} s")
            if line is None:  # stdout closed: the worker died on this request
                elapsed = time.monotonic() - start
                code = self.proc.wait()
                failure = f"worker died ({_exit(code)})"
                if _oom_kills() > kills:
                    failure += ", killed at the memory cap"
                return self._replace(method, None, elapsed, None, failure)
            try:
                reply = json.loads(line)
            except ValueError:
                continue
            if reply.get("id") == req_id:
                return reply, time.monotonic() - start, _peak(self.proc.pid), None

    def _replace(self, method: str, *result):
        print(f"{method}: {result[-1]}", file=sys.stderr)
        if method == "engine_info":
            sys.exit(f"the worker failed to start: {result[-1]}")
        self._start()
        return result

    def close(self) -> None:
        self.proc.stdin.close()
        self.proc.wait(timeout=10)


def _reset_peak(pid: int) -> None:
    """Resets the process's peak RSS (VmHWM), so each request reports its own."""
    try:
        Path(f"/proc/{pid}/clear_refs").write_text("5")
    except OSError:
        pass


def _exit(code: int) -> str:
    return f"signal {-code}" if code < 0 else f"exit status {code}"


def _oom_kills() -> int:
    """
    OOM kills so far in this process's cgroup, which `scripts/memory-cap.sh` makes the capped
    scope holding the script and its worker; 0 where cgroup v2 doesn't say.
    """
    try:
        group = Path("/proc/self/cgroup").read_text().strip().split("::", 1)[1]
        events = Path(f"/sys/fs/cgroup{group}/memory.events").read_text().split()
        return int(events[events.index("oom_kill") + 1])
    except OSError, IndexError, ValueError:
        return 0


def _peak(pid: int) -> int:
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmHWM:"):
                return int(line.split()[1]) * 1024
    except OSError:
        pass
    return 0


def open_library(root: Path) -> sqlite3.Connection:
    db = root / ".opendam" / "library.sqlite"
    conn = sqlite3.connect(f"{db.as_uri()}?mode=ro", uri=True, timeout=20)
    conn.row_factory = sqlite3.Row
    return conn


def pack_root(conn: sqlite3.Connection, file_path: str) -> str | None:
    folder = file_path.rsplit("/", 1)[0] if "/" in file_path else ""
    row = conn.execute(PACK_ROOT, (folder,)).fetchone()
    return None if row is None else row[0]


def fbx_rows(conn: sqlite3.Connection, where: str, params: tuple) -> list[sqlite3.Row]:
    return conn.execute(
        f"SELECT a.id, a.file_path, a.size_bytes, ({APP_PREVIEWS}) AS app_previews"
        f" FROM assets a WHERE lower(a.extension) = 'fbx' AND {where}",
        params,
    ).fetchall()


def measure(worker: Worker, conn, root: Path, out: Path, row) -> dict:
    """
    One `model_meta` request built as `preview::model_meta_request` builds it. The file is named
    on stderr first, so a run the memory cap kills shows which file it was on.
    """
    print(f"measuring {row['file_path']}", file=sys.stderr, flush=True)
    params = {
        "source": str(root / row["file_path"]),
        "output_dir": str(out),
        "name": row["id"],
        "library_root": str(root),
    }
    pack = pack_root(conn, row["file_path"])
    if pack is not None:
        params["pack_root"] = str(root / pack) if pack else str(root)
    reply, seconds, peak, failure = worker.call("model_meta", params, timeout=JOB_TIMEOUT)
    result = (reply or {}).get("result") or {}
    error = failure  # a timeout or a dead worker wins: it is the finding
    if failure is None and "error" in reply:
        error = reply["error"]
    elif failure is None and "preview_model" not in result:
        error = result.get("preview_error") or result.get("error") or "no preview"
    if error:
        # Without the file's own path, so the pack's row can count alike failures together
        error = error.replace(params["source"], "<file>").strip()
    print(f"  {seconds:.1f} s{f', {failure}' if failure else ''}", file=sys.stderr, flush=True)
    return {
        "file": row["file_path"],
        "size_bytes": row["size_bytes"],
        "app_previews": bool(row["app_previews"]),
        "pack_root": pack,
        "seconds": round(seconds, 3),
        "peak_rss": peak,
        "error": error,
        "failure": failure,
        "report": result.get("preview_textures"),
        "params": params,
    }


def alpha_textures(result: dict) -> list[dict]:
    """
    The alpha textures a measured model used, each with its source size and alpha class,
    resolved in this process with the worker's own functions. Only called for a model whose
    report counts a `cutout` or `ignored` texture.
    """
    import assimp_py
    from PIL import Image

    from textures import bounds, diffuse_reference, load_texture, resolve_texture

    p = result["params"]
    try:
        ai_scene = assimp_py.import_file(p["source"], assimp_py.Process_Triangulate)
    except Exception:
        return []
    where = bounds(p["source"], p["library_root"], p.get("pack_root"))
    single = len(ai_scene.materials) == 1
    found, files = {}, {}
    for material in ai_scene.materials:
        hit = resolve_texture(
            diffuse_reference(material), p["source"], where, material.get("NAME"), single, files
        )
        if hit is None or hit[0] in found:
            continue
        texture = load_texture(hit[0])
        if texture.status == "ok" and texture.alpha != "none":
            with Image.open(hit[0]) as img:
                size = img.size
            found[hit[0]] = {
                "texture": os.path.relpath(hit[0], p["library_root"]),
                "size": f"{size[0]}x{size[1]}",
                "long_side": max(size),
                "alpha": texture.alpha,
            }
    return list(found.values())


def summarize(results: list[dict]) -> dict:
    loaded = [r for r in results if r["error"] is None]
    reports = [r["report"] or {} for r in loaded]
    via, alpha, missing, no_ref = Counter(), Counter(), Counter(), Counter()
    reasons = Counter()
    for rep in reports:
        via.update(rep.get("via", {}))
        alpha.update(rep.get("alpha", {}))
        for name in rep.get("missing", []):
            if name.startswith("material:"):
                reasons["no reference"] += 1
                no_ref[name.removeprefix("material:")] += 1
            else:
                reasons["missing"] += 1
                missing[name] += 1
        reasons["unreadable"] += len(rep.get("unreadable", []))
        reasons["too large"] += len(rep.get("too_large", []))
        reasons["no UVs"] += rep.get("no_uv", 0)
        reasons["no root"] += bool(rep.get("no_root"))
    textured = [rep.get("textured", 0) for rep in reports]
    materials = [rep.get("materials", 0) for rep in reports]
    times = [r["seconds"] for r in results]
    return {
        "sampled": len(results),
        "app_previews": sum(r["app_previews"] for r in results),
        "loaded": len(loaded),
        "failed": Counter(r["error"] for r in results if r["error"]),
        "textured_all": sum(1 for t, m in zip(textured, materials) if m and t == m),
        "textured_some": sum(1 for t, m in zip(textured, materials) if 0 < t < m),
        "color_only": sum(1 for t in textured if t == 0),
        "via": via,
        "reasons": +reasons,
        "missing": missing,
        "no_reference": no_ref,
        "alpha": alpha,
        "median_s": statistics.median(times) if times else 0,
        "max_s": max(times, default=0),
        "peak_rss": max((r["peak_rss"] or 0 for r in results), default=0),
    }


def counts(counter: Counter, top: int | None = None) -> str:
    return ", ".join(f"{k} {v}" for k, v in counter.most_common(top)) or "-"


def mb(n: int | None) -> str:
    return "unknown" if n is None else f"{n / 2**20:.0f} MB"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("library_root", type=Path)
    parser.add_argument("--pack", action="append", default=[], help="pack folder, relative")
    parser.add_argument("--sample", type=int, default=40, help="FBX sampled per pack")
    parser.add_argument("--largest", type=int, default=3, help="largest FBX of the library")
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--out", type=Path, required=True, help="scratch folder for the GLBs")
    parser.add_argument("--json", type=Path, help="also write every result here")
    args = parser.parse_args()

    sys.path.insert(0, str(ENGINE))  # for `alpha_textures`
    root = args.library_root.resolve()
    out = args.out.resolve()
    if out == root or root in out.parents:
        sys.exit(f"--out must lie outside the library: {out}")
    out.mkdir(parents=True, exist_ok=True)
    conn = open_library(root)
    rng = random.Random(args.seed)

    with open(out / "worker.log", "a") as log:
        worker = Worker(log)
        packs = {}
        for pack in args.pack:
            pack = pack.strip("/")
            rows = fbx_rows(conn, "substr(a.file_path, 1, length(?1) + 1) = ?1 || '/'", (pack,))
            rows = sorted(rows, key=lambda r: r["file_path"])
            sample = rng.sample(rows, min(args.sample, len(rows)))
            results = []
            for row in sorted(sample, key=lambda r: r["file_path"]):
                results.append(measure(worker, conn, root, out, row))
            for r in results:
                alpha = (r["report"] or {}).get("alpha", {})
                if alpha.get("cutout") or alpha.get("ignored"):
                    r["alpha_textures"] = alpha_textures(r)
            packs[pack] = {"fbx_in_pack": len(rows), "results": results}

        largest = fbx_rows(conn, "1 ORDER BY a.size_bytes DESC LIMIT ?1", (args.largest,))
        large = [measure(worker, conn, root, out, row) for row in largest]
        worker.close()

    print(f"## {root.name}\n")
    print(
        "| Pack | FBX (sampled / in pack) | App previews | Loaded | Textured all / some"
        " | Color only | via | Untextured materials | Top missing | No-reference materials"
        " | alpha | Time median / max | Worker peak RSS |"
    )
    print("|---" * 13 + "|")
    for pack, data in packs.items():
        s = summarize(data["results"])
        failed = f" ({counts(s['failed'], 2)})" if s["failed"] else ""
        print(
            f"| {pack} | {s['sampled']} / {data['fbx_in_pack']} | {s['app_previews']}"
            f" | {s['loaded']}{failed} | {s['textured_all']} / {s['textured_some']}"
            f" | {s['color_only']} | {counts(s['via'])} | {counts(s['reasons'])}"
            f" | {counts(s['missing'], 5)} | {counts(s['no_reference'], 3)}"
            f" | {counts(+s['alpha'])} | {s['median_s']:.2f} / {s['max_s']:.2f} s"
            f" | {mb(s['peak_rss'])} |"
        )

    alpha_rows = {
        (pack, t["texture"]): t
        for pack, data in packs.items()
        for r in data["results"]
        for t in r.get("alpha_textures", [])
    }
    if alpha_rows:
        print("\n| Pack | Alpha texture | Source size | alpha |\n|---|---|---|---|")
        ordered = sorted(alpha_rows.items(), key=lambda kv: (kv[0][0], -kv[1]["long_side"]))
        for (pack, name), t in ordered:
            print(f"| {pack} | {name.removeprefix(pack + '/')} | {t['size']} | {t['alpha']} |")

    if large:
        print("\n| Largest FBX | Size | Time (60 s timeout) | Worker peak RSS | Textured |")
        print("|---|---|---|---|---|")
    for r in large:
        rep = r["report"] or {}
        failure = r["failure"] or ""
        verdict = failure.split(" (")[0] if failure else "within"
        textured = (
            r["error"]
            if r["error"]
            else f"{rep.get('textured', 0)} of {rep.get('materials', 0)} materials"
            f" ({counts(Counter(rep.get('via', {})))})"
        )
        print(
            f"| {r['file']} | {mb(r['size_bytes'])} | {r['seconds']:.1f} s, {verdict}"
            f" | {mb(r['peak_rss'])} | {textured} |"
        )

    if args.json:
        args.json.write_text(json.dumps({"packs": packs, "largest": large}, indent=1))


if __name__ == "__main__":
    main()
