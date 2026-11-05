"""The second gate's own checks (plan "After the first gate", item 15), read-only.

    python3 -I gate2_check.py <trial folder> [--proposed <out>]

Beside gate_check.py, which reads each stage's runs, and gate_plan_check.py, which reads the
leftovers, this checks what the work since the first gate must have changed, from the GLBs' JSON
chunks, the library DBs and the stage logs:

- The 11 packs against baseline-v4.tsv through pack-map.tsv: every pack matched, only the
  Built-In absences, and lost maps and geometry changes exactly the plan's expected list, the
  two Piece 1 models and the 15 Fantasy Adventure trees of its table.
- POLYGON's 162 grid GLBs carry the yellow factor, and its window glass the cyan.
- Fantasy Adventure's 15 trees have no billboard mesh, and the 3 branch materials are `MASK`.
- Every primitive slot-order.tsv lists has its `after` material.
- LostRuins has 102 GLBs, `SM_column04` among them, and Waterways 161, 111 of them textured, and
  neither redirector a GLB.
- No dialog tree warns about a hoist but an exact tree's (Wizard Room's), and no run warns about
  one.

Then it prints each pack's measured textured count as its proposed floor, and with --proposed
writes them as a floors file. The exit status is 1 when a check fails.
"""

import argparse
import csv
import json
import re
import sqlite3
import sys
from collections import Counter
from pathlib import Path

HERE = Path(__file__).resolve()
REPO = HERE.parents[4]
PLAN_DIR = REPO / "docs/plans/import-material-fixes"
sys.path.insert(0, str(REPO / "packages/preview-engine/tools"))

import glb_audit  # noqa: E402

POLYGON = "Synty Studios/POLYGON - Prototype Pack - Art by Synty"
FAE = "Staggart Creations/Fantasy Adventure Environment"
# The pack of Piece 1's two expected geometry changes, which the plan's sentence doesn't name.
VILLAGE = "Stylarts/Stylized Village"
LOST_RUINS = "Fab/Lost Ruins"
WATERWAYS = "Fab/Waterways"
# Item 1's measured factors, linear, to the 4 places the plan gives them.
GRID = ("PolygonPrototype_Grid_01", [1, 0.4831, 0, 1], 162)
GLASS = ("PolygonPrototype_Glass_01", [0.0051, 0.6596, 0.8714, 1], 1)
BRANCHES = ("FAE_WillowBranch", "FAE_PalmBranch", "FAE_PineBranch")
# Item 7's counts, and item 12's textured count, which a lower one must explain.
UNREAL = {
    LOST_RUINS: {"glbs": 102, "has": ["SM_column04"], "lacks": []},
    WATERWAYS: {"glbs": 161, "textured": 111, "has": [], "lacks": ["SM_Cylinder", "Cylinder1"]},
}
NUMBER = r"([\d,]+)"


def bare(name: str) -> str:
    return re.sub(r"\.\d+$", "", name)


def number(text: str) -> int:
    return int(text.replace(",", ""))


class Checks:
    def __init__(self) -> None:
        self.failed = 0

    def check(self, ok: bool, what: str, detail: str = "") -> None:
        self.failed += not ok
        print(f"{'PASS' if ok else 'FAIL'}: {what}{f' ({detail})' if detail else ''}")


def expected_changes(plan: list[str]) -> tuple[dict, dict]:
    """The plan's expected geometry, by pack and GLB name: (primitives, vertices, meshes) before and
    after, meshes None where the plan doesn't state them, and the Fantasy Adventure GLBs that lose
    a map with their billboard."""
    geometry = {}
    text = "\n".join(plan)
    piece1 = re.search(r"The expected list\s+is exactly (.+?)\. Any other", text, re.S)
    for glb, v0, v1, p0, p1 in re.findall(
        rf"`(\w+\.glb)` \({NUMBER} to {NUMBER} vertices, {NUMBER} to {NUMBER} primitives?\)",
        piece1[1],
    ):
        geometry[(VILLAGE, glb)] = ((number(p0), number(p1)), (number(v0), number(v1)), None)
    at = [line.strip() for line in plan].index(
        "| GLB | Vertices | Primitives | Lost with the billboard |"
    )
    loses = set()
    for line in plan[at + 2 :]:
        if not line.lstrip().startswith("|"):
            break
        glb, vertices, primitives, lost = (c.strip() for c in line.strip().strip("|").split("|"))
        glb = glb.strip("`")
        v0, v1 = (number(v) for v in vertices.split(" to "))
        p0, p1 = (number(p) for p in primitives.split(" to "))
        geometry[(FAE, glb)] = ((p0, p1), (v0, v1), (2, 1))
        if lost.startswith("Base and normal"):
            loses.add(glb)
    return geometry, loses


def check_compare(checks: Checks, current: glb_audit.Manifest, plan: list[str]) -> None:
    baseline = glb_audit.read_manifest(PLAN_DIR / "baseline-v4.tsv")
    pack_map = glb_audit.read_pack_map(PLAN_DIR / "pack-map.tsv")
    result = glb_audit.compare(baseline, current, pack_map)
    checks.check(
        not result.short_packs, "every pack matches its map count", "; ".join(result.short_packs)
    )
    checks.check(
        not result.unmatched_baseline and not result.unmatched_current,
        "no unmatched GLB",
        f"{len(result.unmatched_baseline)} baseline, {len(result.unmatched_current)} current",
    )
    absent = sum(result.expected_absent.values())
    checks.check(absent == 193, "only the 193 Built-In GLBs absent", f"{absent}")

    geometry, loses = expected_changes(plan)
    # By pack and file name, as the plan names them. Two changed GLBs of one name in one pack
    # can't both be the plan's row, so the second is unexpected rather than overwriting the first.
    found = set()
    unexpected = []
    for line in result.geometry:
        pack, path, change = line.split("\t")
        m = re.match(
            r"meshes (\d+) to (\d+), primitives (\d+) to (\d+), vertices (\d+) to (\d+)", change
        )
        m0, m1, p0, p1, v0, v1 = map(int, m.groups())
        key = (pack, Path(path).name)
        want = geometry.get(key)
        if key in found:
            unexpected.append(f"{pack}/{path}: a second changed GLB named {key[1]}")
        elif want is None or want[0] != (p0, p1) or want[1] != (v0, v1):
            unexpected.append(f"{pack}/{path}: {change_text(m0, m1, p0, p1, v0, v1)}")
        elif want[2] is not None and want[2] != (m0, m1):
            unexpected.append(f"{pack}/{path}: meshes {m0} to {m1}")
        found.add(key)
    missing = sorted(set(geometry) - found)
    checks.check(
        not unexpected and not missing,
        f"geometry changes are exactly the plan's {len(geometry)}",
        "; ".join(unexpected + [f"{pack}/{glb} unchanged" for pack, glb in missing]),
    )

    lost = Counter()
    wrong = []
    for line in result.lost_maps:
        pack, path, loss = line.split("\t")
        glb = Path(path).name
        drop = re.fullmatch(r"(base|normal): (\d+) to (\d+) materials", loss)
        if pack == FAE and glb in loses and drop and int(drop[2]) - int(drop[3]) == 1:
            lost[(glb, drop[1])] += 1
        else:
            wrong.append(f"{glb}: {loss}")
    expected = Counter({(glb, role): 1 for glb in loses for role in ("base", "normal")})
    checks.check(
        not wrong and lost == expected,
        f"lost maps are exactly the {sum(expected.values())} billboard maps the plan expects",
        "; ".join(wrong + [f"{g} {r} not lost" for (g, r) in expected - lost]),
    )
    print(f"  {len(result.lost_maps)} lost maps, {len(result.geometry)} geometry changes")


def change_text(m0, m1, p0, p1, v0, v1) -> str:
    return f"meshes {m0} to {m1}, primitives {p0} to {p1}, vertices {v0} to {v1}"


def glb_json(libs: Path, pack: str, path: str) -> dict:
    for library in libs.iterdir():
        file = library / pack / path
        if file.is_file():
            return glb_audit.read_glb_json(file)
    raise glb_audit.AuditError(f"no {pack}/{path} in {libs}")


def used_materials(gltf: dict) -> list[dict]:
    materials = gltf.get("materials", [])
    used = {p.get("material", -1) for mesh in gltf.get("meshes", []) for p in mesh["primitives"]}
    return [materials[i] for i in sorted(used) if i >= 0]


def check_factors(checks: Checks, libs: Path, current: glb_audit.Manifest) -> None:
    for name, factor, count in (GRID, GLASS):
        glbs = [
            p for p, glb in current[POLYGON].items() if any(bare(m) == name for m in glb.materials)
        ]
        off = []
        for path in glbs:
            for material in used_materials(glb_json(libs, POLYGON, path)):
                if bare(material.get("name", "")) != name:
                    continue
                got = material.get("pbrMetallicRoughness", {}).get("baseColorFactor", [1, 1, 1, 1])
                if any(abs(a - b) > 5e-4 for a, b in zip(got, factor)):
                    off.append(f"{Path(path).name} {[round(g, 4) for g in got]}")
        checks.check(
            len(glbs) == count and not off,
            f"{count} POLYGON GLBs with {name} carry {factor}",
            f"{len(glbs)} GLBs; " + "; ".join(off[:5]) if off or len(glbs) != count else "",
        )


def check_billboards(
    checks: Checks, libs: Path, current: glb_audit.Manifest, plan: list[str]
) -> None:
    trees, _ = expected_changes(plan)
    trees = {glb for (pack, glb), (_, _, meshes) in trees.items() if pack == FAE and meshes}
    kept = []
    seen = set()
    for path in current[FAE]:
        gltf = glb_json(libs, FAE, path)
        names = [bare(m.get("name", "")) for m in gltf.get("meshes", [])]
        names += [bare(n.get("name", "")) for n in gltf.get("nodes", []) if "mesh" in n]
        billboards = [n for n in names if n.lower().endswith("_billboard")]
        if Path(path).name in trees:
            seen.add(Path(path).name)
        if billboards and len(gltf.get("meshes", [])) > 1:
            kept.append(f"{Path(path).name}: {billboards[0]}")
    checks.check(
        not kept and seen == trees,
        f"the {len(trees)} trees are there, and no FAE GLB keeps a billboard beside a mesh",
        "; ".join(kept + [f"{t} missing" for t in sorted(trees - seen)]),
    )
    modes = {}
    for glb in current[FAE].values():
        for name, m in glb.materials.items():
            if bare(name) in BRANCHES:
                modes.setdefault(bare(name), set()).add(m["alpha"])
    checks.check(
        all(modes.get(b) == {"MASK"} for b in BRANCHES),
        "the 3 branch materials are MASK on every GLB",
        ", ".join(f"{b} {sorted(modes.get(b, []))}" for b in BRANCHES),
    )


def triangles(gltf: dict, primitive: dict) -> int:
    accessors = gltf["accessors"]
    if "indices" in primitive:
        return accessors[primitive["indices"]]["count"] // 3
    return accessors[primitive["attributes"]["POSITION"]]["count"] // 3


def check_slots(checks: Checks, libs: Path) -> None:
    with (PLAN_DIR / "slot-order.tsv").open(encoding="utf-8", newline="") as table:
        rows = list(csv.DictReader(table, delimiter="\t"))
    wanted = Counter(
        (r["pack"], r["glb"], r["mesh"], int(r["triangles"]), r["after"]) for r in rows
    )
    missing = []
    for pack, glb in sorted({(r["pack"], r["glb"]) for r in rows}):
        gltf = glb_json(libs, pack, glb)
        materials = gltf.get("materials", [])
        have = Counter()
        for mesh in gltf.get("meshes", []):
            for p in mesh["primitives"]:
                material = bare(materials[p["material"]].get("name", "")) if "material" in p else ""
                have[(mesh.get("name", ""), triangles(gltf, p), material)] += 1
        for (wp, wg, mesh, tris, after), count in wanted.items():
            if (wp, wg) == (pack, glb) and have[(mesh, tris, bare(after))] < count:
                missing.append(f"{Path(glb).name} {mesh} {tris} triangles: not {after}")
    glbs = len({(r["pack"], r["glb"]) for r in rows})
    checks.check(
        not missing,
        f"all {len(rows)} slot-order.tsv primitives, in {glbs} GLBs, have their after material",
        f"{len(missing)} don't, such as " + "; ".join(missing[:3]) if missing else "",
    )


def check_unreal(checks: Checks, unreal: glb_audit.Manifest) -> None:
    for pack, want in UNREAL.items():
        glbs = unreal.get(pack)
        if glbs is None:
            checks.check(False, f"{pack} is built")
            continue
        total, textured = glb_audit.pack_counts(glbs)
        stems = {Path(path).stem for path in glbs}
        checks.check(total == want["glbs"], f"{pack} has {want['glbs']} GLBs", f"{total}")
        if "textured" in want:
            checks.check(
                textured >= want["textured"],
                f"{pack} reads at least {want['textured']} textured under item 12's audit",
                f"{textured} of {total}",
            )
        for stem in want["has"]:
            checks.check(stem in stems, f"{pack} has {stem}.glb")
        for stem in want["lacks"]:
            checks.check(stem not in stems, f"{pack} has no {stem}.glb, a redirector")


def check_hoists(checks: Checks, folder: Path) -> None:
    trees = []
    for log in sorted((folder / "h").glob("log-*.jsonl")):
        for line in log.read_text(encoding="utf-8").splitlines():
            entry = json.loads(line)
            if entry["k"] != "tree":
                continue
            for candidate in entry["v"]["candidates"]:
                for warning in candidate.get("warnings", []):
                    if "hoist" in warning:
                        trees.append((candidate["target"], candidate["estimate"], warning))
    estimates = [f"{t}: {w}" for t, estimate, w in trees if estimate]
    for target, _, warning in trees:
        print(f"  dialog warning: {target}: {warning}")
    checks.check(not estimates, "no estimate's dialog warns about a hoist", "; ".join(estimates))
    warned = []
    for db in sorted((folder / "libs").glob("*/.opendam/library.sqlite")):
        conn = sqlite3.connect(f"{db.resolve().as_uri()}?mode=ro", uri=True)
        for runs_packs, warnings in conn.execute("SELECT packs, warnings FROM import_runs"):
            for warning in json.loads(warnings):
                if warning.get("reason") == "noHoistFolder":
                    warned.append(f"{runs_packs}: {warning['sourcePath']}")
        conn.close()
    checks.check(not warned, "no run warns about a hoist", "; ".join(warned))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("folder", type=Path)
    parser.add_argument("--proposed", type=Path, help="write the proposed floors here")
    args = parser.parse_args()
    libs = args.folder / "libs"
    plan = (PLAN_DIR / "plan.md").read_text(encoding="utf-8").splitlines()
    pack_map = glb_audit.read_pack_map(PLAN_DIR / "pack-map.tsv")
    unity = [row.current_pack for row in pack_map]
    roots = [libs / "Fab", libs / "Unity"]
    current = glb_audit.build_manifest(roots, unity)
    built = set(glb_audit.library_packs(libs / "Fab"))
    unreal = (
        glb_audit.build_manifest([libs / "Fab"], sorted(built & set(UNREAL)))
        if built & set(UNREAL)
        else {}
    )

    checks = Checks()
    print("== The 11 packs against the baseline")
    check_compare(checks, current, plan)
    print("\n== Converter fixes")
    check_factors(checks, libs, current)
    check_billboards(checks, libs, current, plan)
    check_slots(checks, libs)
    print("\n== Unreal packs")
    check_unreal(checks, unreal)
    print("\n== Hoists")
    check_hoists(checks, args.folder)

    print("\n== Proposed floors, the measured textured counts")
    with (PLAN_DIR / "floors.tsv").open(encoding="utf-8", newline="") as table:
        floors = {row["pack"]: int(row["floor"]) for row in csv.DictReader(table, delimiter="\t")}
    proposed = []
    for pack in [*unity, *sorted(unreal)]:
        total, textured = glb_audit.pack_counts({**current, **unreal}[pack])
        today = floors.get(pack)
        if today is not None:
            checks.check(textured >= today, f"{pack} holds its floor of {today}", f"{textured}")
        print(
            f"  {pack}\t{total} GLBs\t{textured} textured\t"
            f"floor today {'-' if today is None else today}"
        )
        proposed.append((pack, textured))
    if args.proposed:
        with args.proposed.open("w", encoding="utf-8", newline="") as out:
            writer = csv.writer(out, delimiter="\t", lineterminator="\n")
            writer.writerow(["pack", "floor"])
            writer.writerows(proposed)
    print(f"\n{checks.failed} checks failed")
    return 1 if checks.failed else 0


if __name__ == "__main__":
    sys.exit(main())
