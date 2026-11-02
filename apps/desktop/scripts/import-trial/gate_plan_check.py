"""Compares a gate's 11 packs with the plan's own tables, read-only.

    python3 -I gate_plan_check.py <manifest> [<plan> <floors>]

Prints the plan's Target column beside floors.tsv and the measured counts, then each leftover GLB
of each pack with the plan's reason for it, by the leftover table in "Piece 1 result", matching a
backticked name of a row against the material, less Blender's numeric suffix, or the GLB's name.
The exit status is 1 on a floor that differs from the plan, a missed floor, a pack whose leftover
GLBs differ from the plan's and that the plan doesn't expect to differ, or a leftover with no
reason, or a count that differs from the one the plan's row states. A difference the plan itself
explains is named, with the plan's line.
"""

import csv
import re
import sys
from collections import Counter, defaultdict
from fnmatch import fnmatchcase
from pathlib import Path

HERE = Path(__file__).resolve()
PLAN_DIR = HERE.parents[4] / "docs/plans/import-material-fixes"
# The plan's short names, with the pack a gate builds.
PACKS = {
    "Stylized Village": "Stylarts/Stylized Village",
    "Ancient Ruins": "Leartes Studios/Ancient Ruins",
    "Spooky Cemetery Crypt": "Stylarts/Spooky Cemetery Crypt",
    "Stylized Elven Valley": "Stylarts/Stylized Elven Valley",
    "Stylized Lost Cave": "Stylarts/Stylized Lost Cave",
    "Stylized Wizard Room": "Stylarts/Stylized Wizard Room",
    "Elven Long Bow": "Manuel Wieser/Elven Long Bow",
    "POLYGON Prototype (Synty)": "Synty Studios/POLYGON - Prototype Pack - Art by Synty",
    "POLYGON Prototype": "Synty Studios/POLYGON - Prototype Pack - Art by Synty",
    "The Big Castle Kit": "Triplebrick/The Big Castle Kit",
    "Fantasy Adventure Environment": "Staggart Creations/Fantasy Adventure Environment",
    "Fantasy Adventure": "Staggart Creations/Fantasy Adventure Environment",
    "Medieval Houses Vol 1, after Piece 2": "GameAssetFactory/Medieval Houses Modular Vol 1",
    "Medieval Houses": "GameAssetFactory/Medieval Houses Modular Vol 1",
}
MEDIEVAL = PACKS["Medieval Houses"]


def table_rows(lines: list[str], start: int) -> list[list[str]]:
    rows = []
    for line in lines[start:]:
        if not line.startswith("|"):
            break
        rows.append([cell.strip() for cell in line.strip().strip("|").split("|")])
    return rows[2:]


def plan_targets(lines: list[str]) -> dict[str, int]:
    at = lines.index("| Pack | GLBs | Now | Target (floor) |")
    targets = {}
    for pack, *_, target in table_rows(lines, at):
        if pack in PACKS and (found := re.match(r"(\d+)", target)):
            targets[PACKS[pack]] = int(found[1])
    return targets


def plan_leftovers(lines: list[str]) -> dict[str, list[tuple[str, str]]]:
    """Each pack's plan rows, as (reason, materials cell), and its GLBs column under `GLBS`."""
    at = lines.index("| Pack | GLBs | Reason | Materials, GLBs |")
    rows = defaultdict(list)
    pack = None
    for name, glbs, reason, cell in table_rows(lines, at):
        if name:
            pack = PACKS[name]
            rows[pack].append(("GLBS", glbs))
        rows[pack].append((reason, cell))
    return rows


def manifest_leftovers(path: Path) -> dict[str, dict[str, set[str]]]:
    """Each pack's GLBs not fully textured, with the materials that make them so."""
    packs = defaultdict(lambda: defaultdict(set))
    with path.open(encoding="utf-8", newline="") as table:
        for row in csv.DictReader(table, delimiter="\t"):
            if not row["base"]:
                packs[row["pack"]][row["path"]].add(row["material"])
    return packs


def bare(material: str) -> str:
    return re.sub(r"\.\d+$", "", material)


def reason_for(rows: list[tuple[str, str]], material: str, path: str) -> tuple[str, str] | None:
    glb = Path(path).stem
    level = re.sub(r"_LOD\d+$", "", glb)
    named = [(reason, cell) for reason, cell in rows if reason != "GLBS"]
    for reason, cell in named:
        names = {material, bare(material), glb}
        if "their LODs" in cell:
            names.add(level)
        for token in re.findall(r"`([^`]+)`", cell):
            if any(fnmatchcase(name, token) for name in names):
                return reason, token
    for reason, cell in named:
        if "Billboard" in material and "billboards" in cell:
            return reason, "billboards"
        if material == "(no material)" and "a mesh that has no material" in cell:
            return reason, "a mesh that has no material"
        if material == "(no mesh)" and reason == "Collider-only":
            return reason, "collider-only"
    for reason, cell in named:
        if material == "(no material)" and reason != "Collider-only" and "no material" in cell:
            return reason, "no material"
    return None


def stated_counts(cell: str) -> dict[str, int]:
    """The GLBs a plan cell states: a name's own count, and "1 each" for the names before it."""
    counts = {token: int(count) for token, count in re.findall(r"`([^`]+)` (\d+)", cell)}
    if "1 each" in cell:
        for token in re.findall(r"`([^`]+)`", cell.split("1 each")[0]):
            counts.setdefault(token, 1)
    if stated := re.search(r"(\d+) `([^`]+)`", cell):
        counts[stated[2]] = int(stated[1])
    if stated := re.search(r"(\d+) GLBs with a mesh that has no material", cell):
        counts["a mesh that has no material"] = int(stated[1])
    if stated := re.search(r"(\d+) billboards", cell):
        counts["billboards"] = int(stated[1])
    if "and their LODs, " in cell:
        counts = {"SM_Walls2 and SM_Books3_2 with LODs": int(cell.rsplit(", ", 1)[1])}
    return counts


def main() -> int:
    args = [Path(arg) for arg in sys.argv[1:]]
    if not 1 <= len(args) <= 3:
        print(f"usage: {sys.argv[0]} <manifest> [<plan> <floors>]", file=sys.stderr)
        return 2
    manifest = args[0]
    plan = args[1] if len(args) > 1 else PLAN_DIR / "plan.md"
    floors_file = args[2] if len(args) > 2 else PLAN_DIR / "floors.tsv"
    lines = plan.read_text(encoding="utf-8").splitlines()
    with floors_file.open(encoding="utf-8", newline="") as table:
        floors = {row["pack"]: int(row["floor"]) for row in csv.DictReader(table, delimiter="\t")}
    targets = plan_targets(lines)
    planned = plan_leftovers(lines)
    leftovers = manifest_leftovers(manifest)
    with manifest.open(encoding="utf-8", newline="") as table:
        glbs = Counter()
        seen = set()
        for row in csv.DictReader(table, delimiter="\t"):
            if (row["pack"], row["path"]) not in seen:
                seen.add((row["pack"], row["path"]))
                glbs[row["pack"]] += 1

    bad = 0
    print("== Floors: plan Target, floors.tsv, measured GLBs, textured, leftover GLBs")
    for pack, target in targets.items():
        left = len(leftovers[pack])
        textured = glbs[pack] - left
        same = "same" if floors[pack] == target else "DIFFERS"
        met = "met" if textured >= target else "MISSED"
        extra = f", {textured - target} above" if textured > target else ""
        bad += same != "same" or met != "met"
        print(
            f"{pack}: plan {target}, floors.tsv {floors[pack]} ({same}), GLBs {glbs[pack]}, "
            f"textured {textured}, leftover {left}, floor {met}{extra}"
        )

    print("\n== Leftovers against the plan's table")
    for pack in targets:
        rows = planned.get(pack, [])
        expected = int(rows[0][1]) if rows else 0
        print(
            f"\n{pack}: plan lists {expected} leftover GLBs, this gate has {len(leftovers[pack])}"
        )
        by_reason = defaultdict(Counter)
        unmatched = []
        for path, materials in sorted(leftovers[pack].items()):
            for material in sorted(materials):
                found = reason_for(rows, material, path)
                if found:
                    by_reason[found[0]][found[1]] += 1
                else:
                    unmatched.append((material, path))
        for reason, tokens in by_reason.items():
            cells = [text for name, text in rows if name == reason]
            found = dict(tokens)
            if "SM_Walls2" in found:
                found = {"SM_Walls2 and SM_Books3_2 with LODs": sum(found.values())}
            print(f"  {reason}: found {found}")
            for cell in cells:
                print(f"    plan: {cell}")
                for token, count in stated_counts(cell).items():
                    if token in found and found[token] != count:
                        bad += 1
                        print(f"    COUNT DIFFERS: {token} found {found[token]}, plan {count}")
        for material, path in unmatched:
            bad += 1
            print(f"  NO REASON IN THE PLAN: {material} on {path}")
        if len(leftovers[pack]) != expected:
            if pack == MEDIEVAL and not leftovers[pack]:
                print(
                    "  Differs as the plan expects: the 2 were the doubled import, and the "
                    "Outcome table sets 193 of 193 once only the URP copy lands"
                )
            else:
                bad += 1
                print("  LEFTOVER COUNT DIFFERS FROM THE PLAN")

    print("\n== The plan's lines on a pack above its floor")
    for line in lines:
        if "passed its floor" in line or "Six Big Castle GLBs" in line or "The floors stay" in line:
            print(line.strip())
    print(f"\n{bad} problems")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
