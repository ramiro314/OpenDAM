"""
Audits built packs' GLBs against a committed manifest (docs/plans/import-material-fixes/plan.md,
"Matching against the baseline"). Standard library only, and it writes only the `--out` file,
which must lie outside the libraries it reads. A GLB's JSON chunk is all it reads of the GLB.

Write a manifest of every imported pack in some libraries, or of the `--pack` ones only:

    python3 packages/preview-engine/tools/glb_audit.py manifest ~/libs/Fab ~/libs/Unity \\
      --out current.tsv

Compare it with the baseline, through a pack map. The exit status is 1 on a lost map, an unmatched
GLB other than `EXPECTED_ABSENT`, a pack whose matched count differs from the map's, or a missed
floor. A changed vertex, primitive or mesh count is listed for a person to explain, and fails
nothing, because the plan expects some:

    python3 packages/preview-engine/tools/glb_audit.py compare \\
      docs/plans/import-material-fixes/baseline-v4.tsv current.tsv \\
      --pack-map docs/plans/import-material-fixes/pack-map.tsv --floors floors.tsv

Report one manifest's textured counts and leftovers, for packs with no baseline:

    python3 packages/preview-engine/tools/glb_audit.py report current.tsv [--floors floors.tsv]

A floors file is a TSV with the header `pack`, `floor`, one row per current pack. A pack with no
floor fails, as does a floor with no pack.
"""

from __future__ import annotations

import argparse
import csv
import json
import os
import sqlite3
import struct
import sys
from collections import Counter
from collections.abc import Iterable
from contextlib import closing
from dataclasses import dataclass, field
from pathlib import Path

MANIFEST_COLUMNS = [
    "pack",
    "path",
    "meshes",
    "primitives",
    "vertices",
    "material",
    "base",
    "normal",
    "orm",
    "alpha",
]
PACK_MAP_COLUMNS = ["baseline_pack", "baseline_prefix", "current_pack", "current_prefix", "glbs"]
FLOOR_COLUMNS = ["pack", "floor"]
ROLES = ("base", "normal", "orm")

# The `material` cell of a GLB with no mesh, and of a primitive with no material.
NO_MESH = "(no mesh)"
NO_MATERIAL = "(no material)"
# The base Unreal's export bakes from the engine's default grid material, no real texture (plan
# "After the first gate", item 12). A manifest keeps the name, and only the counts ignore it.
ENGINE_GRID = "WorldGridMaterial_BaseColor"

# Baseline GLBs that may have no match: Medieval's Built-In copy, which Piece 2 stops importing.
# The map's GLBs column still has to match, so losing them before Piece 2 fails on the count.
EXPECTED_ABSENT = (
    ("GameAssetFactory/Medieval Houses Modular Vol 1", "Medieval_Houses_Vol1_Built-In/"),
)


def has_base(m: dict[str, str]) -> bool:
    """Whether a material row has a base map, the engine's grid not counting as one."""
    return bool(m["base"]) and m["base"] != ENGINE_GRID


class AuditError(Exception):
    """An input the audit can't trust, such as a wrong header or a broken GLB."""


@dataclass
class Glb:
    """One GLB of a manifest, with a row per material its meshes use."""

    meshes: int
    primitives: int
    vertices: int
    materials: dict[str, dict[str, str]] = field(default_factory=dict)

    def geometry(self) -> tuple[int, int, int]:
        return self.meshes, self.primitives, self.vertices

    def textured(self) -> bool:
        """The plan's "fully textured": every material its meshes use has a base map."""
        return bool(self.materials) and all(has_base(m) for m in self.materials.values())

    def count(self, role: str) -> int:
        return sum(1 for m in self.materials.values() if m[role])


type Manifest = dict[str, dict[str, Glb]]  # pack, then path relative to the pack


@dataclass
class PackRow:
    baseline_pack: str
    baseline_prefix: str
    current_pack: str
    current_prefix: str
    glbs: int

    def side(self, baseline: bool) -> tuple[str, str]:
        if baseline:
            return self.baseline_pack, self.baseline_prefix
        return self.current_pack, self.current_prefix


def read_glb_json(path: Path) -> dict:
    """A GLB's JSON chunk. Anything else at this path is an error, never an empty row."""
    try:
        with path.open("rb") as f:
            magic, _version, _length = struct.unpack("<4sII", f.read(12))
            size, kind = struct.unpack("<I4s", f.read(8))
            if magic != b"glTF" or kind != b"JSON":
                raise ValueError("not a GLB")
            chunk = f.read(size)
            if len(chunk) != size:
                raise ValueError("truncated")
            return json.loads(chunk)
    except (OSError, ValueError, struct.error) as e:
        raise AuditError(f"{path}: can't read its JSON chunk: {e}") from e


def texture_name(gltf: dict, info: dict | None) -> str:
    """The image a `textureInfo` binds, so a map's cell is empty only when there is no map.

    A converted texture's name can be its scratch file's number, so only whether a cell is empty
    means anything to a comparison.
    """
    if info is None:
        return ""
    texture = gltf["textures"][info["index"]]
    source = texture.get("source")
    if source is None:
        return f"texture{info['index']}"
    image = gltf["images"][source]
    return image.get("name") or image.get("uri") or f"image{source}"


def material_row(gltf: dict, material: dict) -> dict[str, str]:
    pbr = material.get("pbrMetallicRoughness", {})
    return {
        "base": texture_name(gltf, pbr.get("baseColorTexture")),
        "normal": texture_name(gltf, material.get("normalTexture")),
        "orm": texture_name(gltf, pbr.get("metallicRoughnessTexture")),
        "alpha": material.get("alphaMode", "OPAQUE"),
    }


def facts(gltf: dict) -> Glb:
    """What a manifest keeps of a GLB's JSON."""
    meshes = gltf.get("meshes", [])
    primitives = [p for mesh in meshes for p in mesh["primitives"]]
    accessors = gltf.get("accessors", [])
    glb = Glb(
        meshes=len(meshes),
        primitives=len(primitives),
        vertices=sum(
            accessors[p["attributes"]["POSITION"]]["count"]
            for p in primitives
            if "POSITION" in p["attributes"]
        ),
    )
    materials = gltf.get("materials", [])
    for index in sorted({p.get("material", -1) for p in primitives}):
        if index < 0:
            glb.materials[NO_MATERIAL] = dict.fromkeys(ROLES, "") | {"alpha": ""}
        else:
            name = materials[index].get("name") or f"material{index}"
            if name in glb.materials:
                name = f"{name}#{index}"
            glb.materials[name] = material_row(gltf, materials[index])
    return glb


def library_packs(root: Path) -> list[str]:
    """A library's imported packs, by target, read without touching its database."""
    db = (root / ".opendam" / "library.sqlite").resolve()
    try:
        # Read-only: a missing database is an error, and DELETE journaling makes no side files.
        with closing(sqlite3.connect(f"{db.as_uri()}?mode=ro", uri=True)) as conn:
            rows = conn.execute("SELECT target FROM import_packs ORDER BY target")
            return [row[0] for row in rows]
    except sqlite3.Error as e:
        raise AuditError(f"{db}: can't read its packs: {e}") from e


def pack_glbs(folder: Path) -> Iterable[Path]:
    for parent, folders, files in os.walk(folder):
        folders[:] = [name for name in folders if not name.startswith(".")]
        for name in files:
            if name.lower().endswith(".glb"):
                yield Path(parent, name)


def build_manifest(roots: list[Path], only: list[str] | None = None) -> Manifest:
    manifest: Manifest = {}
    for root in roots:
        for pack in library_packs(root):
            if only is not None and pack not in only:
                continue
            if pack in manifest:
                raise AuditError(f"pack {pack} is in two libraries")
            manifest[pack] = {
                file.relative_to(root / pack).as_posix(): facts(read_glb_json(file))
                for file in pack_glbs(root / pack)
            }
    missing = sorted(set(only or []) - set(manifest))
    if missing:
        raise AuditError(f"no such pack: {', '.join(missing)}")
    if not any(manifest.values()):
        raise AuditError("no GLB in the given packs")
    return manifest


def write_manifest(manifest: Manifest, out: Path) -> None:
    """One row per GLB and material, sorted, so two runs over the same files are identical."""
    with out.open("w", newline="", encoding="utf-8") as f:
        writer = csv.writer(f, delimiter="\t", lineterminator="\n")
        writer.writerow(MANIFEST_COLUMNS)
        for pack in sorted(manifest):
            for path, glb in sorted(manifest[pack].items()):
                head = [pack, path, glb.meshes, glb.primitives, glb.vertices]
                if not glb.materials:
                    writer.writerow([*head, NO_MESH, "", "", "", ""])
                for name, m in sorted(glb.materials.items()):
                    writer.writerow([*head, name, m["base"], m["normal"], m["orm"], m["alpha"]])


def read_table(path: Path, columns: list[str]) -> list[dict[str, str]]:
    """A TSV with exactly these columns, so a file of the wrong kind stops here."""
    with path.open(newline="", encoding="utf-8") as f:
        reader = csv.DictReader(f, delimiter="\t")
        if reader.fieldnames != columns:
            raise AuditError(f"{path}: expected columns {columns}, found {reader.fieldnames}")
        rows = list(reader)
    if not rows:
        raise AuditError(f"{path}: no rows")
    return rows


def read_int(path: Path, value: str) -> int:
    try:
        return int(value)
    except (TypeError, ValueError) as e:
        raise AuditError(f"{path}: {value!r} is not a count") from e


def read_manifest(path: Path) -> Manifest:
    manifest: Manifest = {}
    for row in read_table(path, MANIFEST_COLUMNS):
        glbs = manifest.setdefault(row["pack"], {})
        glb = glbs.get(row["path"])
        if glb is None:
            glb = glbs[row["path"]] = Glb(
                *(read_int(path, row[c]) for c in ("meshes", "primitives", "vertices"))
            )
        if row["material"] != NO_MESH:
            glb.materials[row["material"]] = {k: row[k] for k in (*ROLES, "alpha")}
    return manifest


def read_pack_map(path: Path) -> list[PackRow]:
    rows = [
        PackRow(
            row["baseline_pack"],
            row["baseline_prefix"],
            row["current_pack"],
            row["current_prefix"],
            read_int(path, row["glbs"]),
        )
        for row in read_table(path, PACK_MAP_COLUMNS)
    ]
    for row in rows:
        for pack in (row.baseline_pack, row.current_pack):
            if not pack:
                raise AuditError(f"{path}: a row names no pack")
        # On whole path components, as the app compares paths: a prefix that stops inside a folder
        # name could still join two sides that agree on the same wrong cut.
        for prefix in (row.baseline_prefix, row.current_prefix):
            if prefix and (not prefix.endswith("/") or prefix.startswith("/")):
                raise AuditError(f"{path}: prefix {prefix!r} must be a relative folder ending in /")
    for name, packs in (
        ("baseline", [row.baseline_pack for row in rows]),
        ("current", [row.current_pack for row in rows]),
    ):
        repeated = [pack for pack, n in Counter(packs).items() if n > 1]
        if repeated:
            raise AuditError(f"{path}: {name} pack in more than one row: {', '.join(repeated)}")
    return rows


def read_floors(path: Path) -> dict[str, int]:
    rows = read_table(path, FLOOR_COLUMNS)
    floors = {row["pack"]: read_int(path, row["floor"]) for row in rows}
    if len(floors) != len(rows):
        raise AuditError(f"{path}: a pack has two floors")
    return floors


@dataclass
class Comparison:
    matched: dict[str, int] = field(default_factory=dict)  # by the map's baseline pack
    lost_maps: list[str] = field(default_factory=list)
    geometry: list[str] = field(default_factory=list)
    unmatched_baseline: list[str] = field(default_factory=list)
    unmatched_current: list[str] = field(default_factory=list)
    expected_absent: Counter = field(default_factory=Counter)
    short_packs: list[str] = field(default_factory=list)

    def failed(self) -> bool:
        return bool(
            self.lost_maps or self.unmatched_baseline or self.unmatched_current or self.short_packs
        )


def expected_absent(pack: str, path: str) -> str | None:
    for absent_pack, prefix in EXPECTED_ABSENT:
        if pack == absent_pack and path.startswith(prefix):
            return prefix
    return None


def lost_maps(before: Glb, after: Glb) -> list[str]:
    """Each map a material of this name lost, and each role the GLB holds fewer of.

    The count catches a slot that lost a map as it resolved to a material of another name. It
    can't tell a slot that lost its map from two slots that came to share one material, so such a
    GLB is listed for a person to judge.
    """
    lost = []
    for role in ROLES:
        names = [
            name
            for name, m in sorted(before.materials.items())
            if m[role] and name in after.materials and not after.materials[name][role]
        ]
        lost.extend(f"{role} of {name}" for name in names)
        if not names and after.count(role) < before.count(role):
            lost.append(f"{role}: {before.count(role)} to {after.count(role)} materials")
    return lost


def joinable(manifest: Manifest, pack: str, prefix: str) -> dict[str, str]:
    """A side's paths by join key, the path after the prefix."""
    return {
        path.removeprefix(prefix): path
        for path in manifest.get(pack, {})
        if path.startswith(prefix)
    }


def compare(baseline: Manifest, current: Manifest, pack_map: list[PackRow]) -> Comparison:
    """Joins on the mapped pack plus the path after the prefix, and judges what each side left.

    Every GLB starts unmatched and only a join clears it, so a pack no row names, or a path
    outside its row's prefix, can't go unnoticed.
    """
    result = Comparison()
    left_baseline = {(pack, path) for pack, glbs in baseline.items() for path in glbs}
    left_current = {(pack, path) for pack, glbs in current.items() for path in glbs}
    for row in pack_map:
        before = joinable(baseline, row.baseline_pack, row.baseline_prefix)
        after = joinable(current, row.current_pack, row.current_prefix)
        shared = sorted(before.keys() & after.keys())
        result.matched[row.baseline_pack] = len(shared)
        if len(shared) != row.glbs:
            result.short_packs.append(
                f"{row.baseline_pack}: {len(shared)} matched, the map says {row.glbs}"
            )
        for key in shared:
            left_baseline.discard((row.baseline_pack, before[key]))
            left_current.discard((row.current_pack, after[key]))
            old = baseline[row.baseline_pack][before[key]]
            new = current[row.current_pack][after[key]]
            label = f"{row.current_pack}\t{after[key]}"
            result.lost_maps.extend(f"{label}\t{loss}" for loss in lost_maps(old, new))
            if old.geometry() != new.geometry():
                result.geometry.append(
                    f"{label}\tmeshes {old.meshes} to {new.meshes}, primitives {old.primitives} "
                    f"to {new.primitives}, vertices {old.vertices} to {new.vertices}"
                )
    for pack, path in sorted(left_baseline):
        prefix = expected_absent(pack, path)
        if prefix is None:
            result.unmatched_baseline.append(f"{pack}\t{path}")
        else:
            result.expected_absent[f"{pack}\t{prefix}"] += 1
    result.unmatched_current = [f"{pack}\t{path}" for pack, path in sorted(left_current)]
    return result


def pack_counts(glbs: dict[str, Glb]) -> tuple[int, int]:
    return len(glbs), sum(1 for glb in glbs.values() if glb.textured())


def leftovers(glbs: dict[str, Glb]) -> Counter:
    """What keeps GLBs from being fully textured: each untextured material, by the GLBs using it."""
    left: Counter = Counter()
    for glb in glbs.values():
        if not glb.materials:
            left[NO_MESH] += 1
        for name, m in glb.materials.items():
            if not has_base(m):
                left[name] += 1
    return left


def report(manifest: Manifest, floors: dict[str, int] | None = None) -> bool:
    """Prints each pack's textured count and leftovers, and returns whether every floor held."""
    ok = True
    print("pack\tGLBs\ttextured\tfloor\tresult")
    for pack, glbs in sorted(manifest.items()):
        total, textured = pack_counts(glbs)
        floor = None if floors is None else floors.get(pack)
        if floors is None:
            print(f"{pack}\t{total}\t{textured}\t-\t-")
        elif floor is None:
            ok = False
            print(f"{pack}\t{total}\t{textured}\t-\tFAIL, no floor")
        else:
            held = textured >= floor
            ok = ok and held
            print(f"{pack}\t{total}\t{textured}\t{floor}\t{'ok' if held else 'FAIL'}")
    for pack in sorted(set(floors or ()) - set(manifest)):
        ok = False
        print(f"{pack}\t-\t-\t{floors[pack]}\tFAIL, no such pack")
    for pack, glbs in sorted(manifest.items()):
        left = leftovers(glbs)
        if left:
            print(f"\nLeftovers in {pack}: material, GLBs")
            for name, count in sorted(left.items(), key=lambda item: (-item[1], item[0])):
                print(f"  {name}\t{count}")
    return ok


def section(title: str, lines: list[str]) -> None:
    print(f"\n{title}: {len(lines)}")
    for line in lines:
        print(f"  {line}")


def print_comparison(result: Comparison, pack_map: list[PackRow]) -> None:
    print("baseline pack\tmatched\tmap")
    for row in pack_map:
        print(f"{row.baseline_pack}\t{result.matched[row.baseline_pack]}\t{row.glbs}")
    section("Packs whose matched count differs from the map", result.short_packs)
    section("Unmatched baseline GLBs", result.unmatched_baseline)
    section("Unmatched current GLBs", result.unmatched_current)
    section(
        "Expected absences",
        [f"{key}\t{count}" for key, count in sorted(result.expected_absent.items())],
    )
    section("Lost maps", result.lost_maps)
    section("Changed vertex, primitive or mesh counts, to explain", result.geometry)


def check_outside(out: Path, roots: list[Path]) -> None:
    for root in roots:
        if out.resolve().is_relative_to(root.resolve()):
            raise AuditError(f"--out must lie outside the library {root}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Audit built packs' GLBs against a manifest.")
    commands = parser.add_subparsers(dest="command", required=True)
    manifest = commands.add_parser("manifest", help="write a manifest of libraries' packs")
    manifest.add_argument("libraries", nargs="+", type=Path, help="library root folders")
    manifest.add_argument("--out", required=True, type=Path)
    manifest.add_argument("--pack", action="append", help="only this pack, as its target")
    comparing = commands.add_parser("compare", help="compare two manifests through a pack map")
    comparing.add_argument("baseline", type=Path)
    comparing.add_argument("current", type=Path)
    comparing.add_argument("--pack-map", required=True, type=Path)
    comparing.add_argument("--floors", type=Path)
    reporting = commands.add_parser("report", help="report one manifest's counts and leftovers")
    reporting.add_argument("manifest", type=Path)
    reporting.add_argument("--floors", type=Path)
    args = parser.parse_args(argv)

    try:
        if args.command == "manifest":
            check_outside(args.out, args.libraries)
            write_manifest(build_manifest(args.libraries, args.pack), args.out)
            return 0
        floors = read_floors(args.floors) if args.floors else None
        if args.command == "report":
            return 0 if report(read_manifest(args.manifest), floors) else 1
        pack_map = read_pack_map(args.pack_map)
        current = read_manifest(args.current)
        result = compare(read_manifest(args.baseline), current, pack_map)
        print_comparison(result, pack_map)
        print()
        floors_held = report(current, floors)
        failed = result.failed() or not floors_held
        print(f"\n{'FAILED' if failed else 'PASSED'}")
        return 1 if failed else 0
    except AuditError as e:
        print(f"glb_audit: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
