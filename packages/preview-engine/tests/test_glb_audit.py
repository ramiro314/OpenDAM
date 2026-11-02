"""`glb_audit.py` and `contact_sheet.py` (plan import-material-fixes, "Matching against the
baseline"): the join on small manifests, the manifest's facts from small GLBs, and the committed
version 4 manifest and pack map against the plan's Now column.
"""

from __future__ import annotations

import json
import sqlite3
import struct
from collections import Counter
from pathlib import Path

import contact_sheet
import glb_audit
import pytest
from glb_audit import Comparison, Glb, PackRow
from PIL import Image

REPO = Path(__file__).resolve().parents[3]
PLAN = REPO / "docs/plans/import-material-fixes"

VILLAGE = "Stylarts/StylizedVillage"
VILLAGE_NOW = "Stylarts/Stylized Village"
MEDIEVAL = "GameAssetFactory/Medieval Houses Modular Vol 1"
BUILT_IN = "Medieval_Houses_Vol1_Built-In/"
URP = "Medieval_Houses_Vol1_URP_HDRP/"

# The plan's Outcome table: GLBs and the Now column, before any converter change.
NOW = {
    "Stylarts/StylizedVillage": (181, 0),
    "Leartes Studios/AncientRuins": (184, 157),
    "Stylarts/SpookyCemeteryCrypt": (100, 99),
    "Stylarts/StylizedElvenValley": (200, 179),
    "Stylarts/StylizedLostCave": (208, 198),
    "Stylarts/StylizedWizardRoom": (219, 201),
    "Manuel Wieser/Elven Long Bow": (2, 1),
    "Synty Studios/POLYGON - Prototype Pack - Art by Synty": (950, 746),
    "Triplebrick/The Big Castle Kit": (293, 226),
    "Staggart Creations/Fantasy Adventure Environment": (36, 26),
    MEDIEVAL: (386, 384),
}


def material(base="b", normal="n", orm="o", alpha="OPAQUE") -> dict[str, str]:
    return {"base": base, "normal": normal, "orm": orm, "alpha": alpha}


def glb(vertices=10, primitives=1, meshes=1, **materials) -> Glb:
    return Glb(meshes, primitives, vertices, materials or {"Stone": material()})


def row(baseline, current=None, glbs=1, baseline_prefix="", current_prefix="") -> PackRow:
    return PackRow(baseline, baseline_prefix, current or baseline, current_prefix, glbs)


def assert_passed(result: Comparison) -> None:
    assert not result.failed(), result


def test_identical_packs_match():
    side = {VILLAGE: {"a.glb": glb(), "b.glb": glb()}}
    result = glb_audit.compare(side, side, [row(VILLAGE, glbs=2)])
    assert_passed(result)
    assert result.matched == {VILLAGE: 2}


def test_a_renamed_pack_joins_through_the_map():
    result = glb_audit.compare(
        {VILLAGE: {"a.glb": glb()}},
        {VILLAGE_NOW: {"a.glb": glb()}},
        [row(VILLAGE, VILLAGE_NOW)],
    )
    assert_passed(result)
    assert result.matched == {VILLAGE: 1}


def test_a_wrong_pack_name_fails_on_both_sides_and_compares_nothing():
    result = glb_audit.compare(
        {VILLAGE: {"a.glb": glb()}},
        {VILLAGE_NOW: {"a.glb": glb()}},
        [row(VILLAGE, "Stylarts/Stylized Vilage")],
    )
    assert result.failed()
    assert result.matched == {VILLAGE: 0}
    assert result.short_packs == [f"{VILLAGE}: 0 matched, the map says 1"]
    assert result.unmatched_baseline == [f"{VILLAGE}\ta.glb"]
    assert result.unmatched_current == [f"{VILLAGE_NOW}\ta.glb"]


def test_a_pack_no_row_names_fails():
    side = {VILLAGE: {"a.glb": glb()}, "Other/Pack": {"b.glb": glb()}}
    result = glb_audit.compare(side, side, [row(VILLAGE)])
    assert result.failed()
    assert result.unmatched_baseline == ["Other/Pack\tb.glb"]
    assert result.unmatched_current == ["Other/Pack\tb.glb"]


def test_a_changed_prefix_joins_on_the_path_after_it():
    result = glb_audit.compare(
        {MEDIEVAL: {f"{URP}Core/a.glb": glb()}},
        {MEDIEVAL: {"Core/a.glb": glb()}},
        [row(MEDIEVAL, baseline_prefix=URP)],
    )
    assert_passed(result)
    assert result.matched == {MEDIEVAL: 1}


def test_a_wrong_prefix_fails_and_compares_nothing():
    result = glb_audit.compare(
        {MEDIEVAL: {f"{URP}Core/a.glb": glb()}},
        {MEDIEVAL: {"Core/a.glb": glb()}},
        [row(MEDIEVAL, baseline_prefix="Medieval_URP/")],
    )
    assert result.failed()
    assert result.matched == {MEDIEVAL: 0}
    assert result.unmatched_baseline == [f"{MEDIEVAL}\t{URP}Core/a.glb"]
    assert result.unmatched_current == [f"{MEDIEVAL}\tCore/a.glb"]


def test_the_built_in_copy_is_the_expected_absence():
    result = glb_audit.compare(
        {MEDIEVAL: {f"{URP}a.glb": glb(), f"{BUILT_IN}a.glb": glb(), f"{BUILT_IN}b.glb": glb()}},
        {MEDIEVAL: {"a.glb": glb()}},
        [row(MEDIEVAL, baseline_prefix=URP)],
    )
    assert_passed(result)
    assert result.expected_absent == Counter({f"{MEDIEVAL}\t{BUILT_IN}": 2})


def test_losing_the_built_in_copy_before_the_map_narrows_fails_on_the_count():
    result = glb_audit.compare(
        {MEDIEVAL: {f"{URP}a.glb": glb(), f"{BUILT_IN}a.glb": glb()}},
        {MEDIEVAL: {f"{URP}a.glb": glb()}},
        [row(MEDIEVAL, glbs=2)],
    )
    assert result.failed()
    assert result.short_packs == [f"{MEDIEVAL}: 1 matched, the map says 2"]


def test_an_absence_anywhere_else_fails():
    result = glb_audit.compare(
        {MEDIEVAL: {f"{URP}a.glb": glb(), f"{URP}b.glb": glb()}},
        {MEDIEVAL: {f"{URP}a.glb": glb()}},
        [row(MEDIEVAL)],
    )
    assert result.failed()
    assert result.unmatched_baseline == [f"{MEDIEVAL}\t{URP}b.glb"]
    assert not result.expected_absent


def test_the_same_folder_in_another_pack_is_no_expected_absence():
    result = glb_audit.compare(
        {VILLAGE: {"a.glb": glb(), f"{BUILT_IN}b.glb": glb()}},
        {VILLAGE: {"a.glb": glb()}},
        [row(VILLAGE)],
    )
    assert result.unmatched_baseline == [f"{VILLAGE}\t{BUILT_IN}b.glb"]


def test_an_extra_glb_fails():
    result = glb_audit.compare(
        {VILLAGE: {"a.glb": glb()}},
        {VILLAGE: {"a.glb": glb(), "new.glb": glb()}},
        [row(VILLAGE)],
    )
    assert result.failed()
    assert result.unmatched_current == [f"{VILLAGE}\tnew.glb"]


def test_a_matched_count_short_of_the_map_fails():
    side = {VILLAGE: {"a.glb": glb()}}
    result = glb_audit.compare(side, side, [row(VILLAGE, glbs=2)])
    assert result.failed()
    assert result.short_packs == [f"{VILLAGE}: 1 matched, the map says 2"]
    assert not result.unmatched_baseline and not result.unmatched_current


def test_a_map_row_with_no_pack_on_either_side_fails():
    result = glb_audit.compare({}, {}, [row(VILLAGE)])
    assert result.failed()
    assert result.matched == {VILLAGE: 0}


@pytest.mark.parametrize("role", ["base", "normal", "orm"])
def test_a_material_that_loses_a_map_is_listed(role):
    before = {VILLAGE: {"a.glb": glb(Stone=material(), Wood=material())}}
    after = {VILLAGE: {"a.glb": glb(Stone=material() | {role: ""}, Wood=material())}}
    result = glb_audit.compare(before, after, [row(VILLAGE)])
    assert result.failed()
    assert result.lost_maps == [f"{VILLAGE}\ta.glb\t{role} of Stone"]


def test_a_map_moved_to_another_material_of_the_glb_still_counts_as_lost():
    before = {VILLAGE: {"a.glb": glb(Stone=material(), Wood=material(normal=""))}}
    after = {VILLAGE: {"a.glb": glb(Stone=material(normal=""), Wood=material())}}
    result = glb_audit.compare(before, after, [row(VILLAGE)])
    assert result.lost_maps == [f"{VILLAGE}\ta.glb\tnormal of Stone"]


def test_a_slot_that_resolves_to_a_new_name_keeps_its_maps():
    before = {VILLAGE: {"a.glb": glb(**{"Material.001": material()})}}
    after = {VILLAGE: {"a.glb": glb(wood_a_beams=material())}}
    assert_passed(glb_audit.compare(before, after, [row(VILLAGE)]))


def test_a_renamed_slot_that_lost_a_map_fails_on_the_glb_count():
    before = {VILLAGE: {"a.glb": glb(**{"Material.001": material()})}}
    after = {VILLAGE: {"a.glb": glb(wood_a_beams=material(orm=""))}}
    result = glb_audit.compare(before, after, [row(VILLAGE)])
    assert result.lost_maps == [f"{VILLAGE}\ta.glb\torm: 1 to 0 materials"]


def test_a_new_map_is_no_loss():
    before = {VILLAGE: {"a.glb": glb(Stone=material(base=""))}}
    after = {VILLAGE: {"a.glb": glb(Stone=material())}}
    assert_passed(glb_audit.compare(before, after, [row(VILLAGE)]))


def test_changed_geometry_is_listed_for_a_person_and_fails_nothing():
    before = {VILLAGE: {"a.glb": glb(vertices=0, primitives=0), "b.glb": glb()}}
    after = {VILLAGE: {"a.glb": glb(vertices=3610, primitives=1), "b.glb": glb()}}
    result = glb_audit.compare(before, after, [row(VILLAGE, glbs=2)])
    assert_passed(result)
    assert result.geometry == [
        f"{VILLAGE}\ta.glb\tmeshes 1 to 1, primitives 0 to 1, vertices 0 to 3610"
    ]


def write_gltf(path: Path, gltf: dict) -> None:
    """A GLB of only a JSON chunk: the audit never reads the binary chunk."""
    chunk = json.dumps(gltf).encode()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(
        struct.pack("<4sII", b"glTF", 2, 20 + len(chunk))
        + struct.pack("<I4s", len(chunk), b"JSON")
        + chunk
    )


GLTF = {
    "accessors": [{"count": 8}, {"count": 4}, {"count": 99}],
    "images": [{"name": "T_Stone"}, {"name": "T_Stone_N"}, {"uri": "orm.png"}],
    "textures": [{"source": 0}, {"source": 1}, {"source": 2}],
    "materials": [
        {
            "name": "Stone",
            "alphaMode": "MASK",
            "normalTexture": {"index": 1},
            "pbrMetallicRoughness": {
                "baseColorTexture": {"index": 0},
                "metallicRoughnessTexture": {"index": 2},
            },
        },
        {"name": "Flat"},
        {"name": "Unused", "pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}},
    ],
    "meshes": [
        {"primitives": [{"attributes": {"POSITION": 0}, "material": 0}]},
        {"primitives": [{"attributes": {"POSITION": 1}, "material": 1}]},
    ],
}


def test_facts_read_counts_and_only_the_materials_meshes_use():
    facts = glb_audit.facts(GLTF)
    assert facts.geometry() == (2, 2, 12)
    assert facts.materials == {
        "Flat": {"base": "", "normal": "", "orm": "", "alpha": "OPAQUE"},
        "Stone": {"base": "T_Stone", "normal": "T_Stone_N", "orm": "orm.png", "alpha": "MASK"},
    }
    assert not facts.textured()


def test_a_glb_with_no_mesh_is_not_textured():
    facts = glb_audit.facts({"asset": {"version": "2.0"}})
    assert facts.geometry() == (0, 0, 0)
    assert not facts.textured()


def test_a_primitive_with_no_material_is_not_textured():
    primitives = [
        {"attributes": {"POSITION": 0}, "material": 0},
        {"attributes": {"POSITION": 1}},
    ]
    facts = glb_audit.facts(GLTF | {"meshes": [{"primitives": primitives}]})
    assert set(facts.materials) == {"Stone", glb_audit.NO_MATERIAL}
    assert not facts.textured()


def test_two_materials_of_one_name_stay_two_rows():
    gltf = GLTF | {
        "materials": [{"name": "Stone"}, {"name": "Stone"}],
    }
    assert set(glb_audit.facts(gltf).materials) == {"Stone", "Stone#1"}


@pytest.mark.parametrize(
    "data",
    [
        b"",
        b"glTF",
        b"not a glb at all, but long enough for a header",
        struct.pack("<4sII", b"glTF", 2, 40) + struct.pack("<I4s", 100, b"JSON") + b"{}",
    ],
)
def test_a_broken_glb_stops_the_audit(tmp_path, data):
    path = tmp_path / "broken.glb"
    path.write_bytes(data)
    with pytest.raises(glb_audit.AuditError, match="broken.glb"):
        glb_audit.read_glb_json(path)


def make_library(root: Path, packs: list[str]) -> None:
    (root / ".opendam").mkdir(parents=True)
    with sqlite3.connect(root / ".opendam" / "library.sqlite") as conn:
        conn.execute("CREATE TABLE import_packs (target TEXT)")
        conn.executemany("INSERT INTO import_packs VALUES (?)", [(pack,) for pack in packs])
        conn.execute("CREATE TABLE assets (file_path TEXT, thumbnail_path TEXT)")
    conn.close()


def test_a_manifest_round_trips_and_reads_only_built_packs_glbs(tmp_path):
    library = tmp_path / "lib"
    make_library(library, ["Pub/Pack"])
    write_gltf(library / "Pub/Pack/Meshes/b.glb", GLTF)
    write_gltf(library / "Pub/Pack/a.GLB", {"asset": {"version": "2.0"}})
    write_gltf(library / "Pub/Pack/.opendam-carry/gone.glb", GLTF)
    write_gltf(library / "Pub/Elsewhere/outside.glb", GLTF)
    out = tmp_path / "manifest.tsv"

    assert glb_audit.main(["manifest", str(library), "--out", str(out)]) == 0

    lines = out.read_text().splitlines()
    assert lines[0].split("\t") == glb_audit.MANIFEST_COLUMNS
    assert [line.split("\t")[1] for line in lines[1:]] == [
        "Meshes/b.glb",
        "Meshes/b.glb",
        "a.GLB",
    ]
    assert lines[3].split("\t")[5] == glb_audit.NO_MESH
    read = glb_audit.read_manifest(out)["Pub/Pack"]
    assert read["Meshes/b.glb"] == glb_audit.facts(GLTF)
    assert read["a.GLB"].geometry() == (0, 0, 0) and not read["a.GLB"].materials


def test_a_manifest_can_name_its_packs(tmp_path):
    library = tmp_path / "lib"
    make_library(library, ["Pub/A", "Pub/B"])
    write_gltf(library / "Pub/A/a.glb", GLTF)
    write_gltf(library / "Pub/B/b.glb", GLTF)
    assert set(glb_audit.build_manifest([library], ["Pub/A"])) == {"Pub/A"}
    with pytest.raises(glb_audit.AuditError, match="no such pack: Pub/C"):
        glb_audit.build_manifest([library], ["Pub/A", "Pub/C"])


def test_a_pack_in_two_libraries_stops_the_manifest(tmp_path):
    for name in ("one", "two"):
        make_library(tmp_path / name, ["Pub/Pack"])
        write_gltf(tmp_path / name / "Pub/Pack/a.glb", GLTF)
    with pytest.raises(glb_audit.AuditError, match="two libraries"):
        glb_audit.build_manifest([tmp_path / "one", tmp_path / "two"])


def test_a_folder_with_no_library_or_no_glb_stops_the_manifest(tmp_path):
    with pytest.raises(glb_audit.AuditError, match="can't read its packs"):
        glb_audit.build_manifest([tmp_path / "nothing"])
    make_library(tmp_path / "lib", ["Pub/Pack"])
    with pytest.raises(glb_audit.AuditError, match="no GLB"):
        glb_audit.build_manifest([tmp_path / "lib"])


def snapshot(root: Path) -> dict[str, tuple[int, bytes]]:
    return {str(p): (p.stat().st_mtime_ns, p.read_bytes()) for p in root.rglob("*") if p.is_file()}


def test_a_manifest_changes_nothing_in_the_library_and_refuses_to_write_there(tmp_path):
    library = tmp_path / "lib"
    make_library(library, ["Pub/Pack"])
    write_gltf(library / "Pub/Pack/a.glb", GLTF)
    before = snapshot(library)

    assert glb_audit.main(["manifest", str(library), "--out", str(tmp_path / "m.tsv")]) == 0
    assert glb_audit.main(["manifest", str(library), "--out", str(library / "m.tsv")]) == 2

    assert snapshot(library) == before


def tsv(path: Path, *lines: str) -> Path:
    path.write_text("\n".join(lines) + "\n")
    return path


def test_a_pack_map_refuses_what_could_compare_nothing(tmp_path):
    header = "\t".join(glb_audit.PACK_MAP_COLUMNS)
    cases = {
        "columns": ("pack\tprefix", "a\tb"),
        "no rows": (header,),
        "count": (header, "a\t\ta\t\tmany"),
        "pack": (header, "a\t\t\t\t1"),
        "folder": (header, "a\tFolder\ta\t\t1"),
        "relative": (header, "a\t/Folder/\ta\t\t1"),
        "baseline pack": (header, "a\t\tb\t\t1", "a\tx/\tc\t\t1"),
        "current pack": (header, "a\t\tc\t\t1", "b\t\tc\t\t1"),
    }
    for name, lines in cases.items():
        with pytest.raises(glb_audit.AuditError):
            glb_audit.read_pack_map(tsv(tmp_path / f"{name}.tsv", *lines))


def test_a_manifest_of_the_wrong_shape_stops_the_comparison(tmp_path):
    with pytest.raises(glb_audit.AuditError, match="expected columns"):
        glb_audit.read_manifest(tsv(tmp_path / "m.tsv", "pack\tfloor", "a\t1"))


def test_a_floors_file_with_two_floors_for_a_pack_stops(tmp_path):
    path = tsv(tmp_path / "f.tsv", "pack\tfloor", "a\t1", "a\t2")
    with pytest.raises(glb_audit.AuditError, match="two floors"):
        glb_audit.read_floors(path)


def floors_manifest() -> dict[str, dict[str, Glb]]:
    return {
        VILLAGE: {
            "done.glb": glb(),
            "water.glb": glb(Water=material(base="")),
            "pond.glb": glb(Water=material(base=""), Stone=material()),
            "fire.glb": glb(Flame=material(base="")),
            "collider.glb": Glb(0, 0, 0),
        }
    }


def test_a_met_floor_passes_and_leftovers_list_by_material_and_count(capsys):
    assert glb_audit.report(floors_manifest(), {VILLAGE: 1})
    out = capsys.readouterr().out
    assert f"{VILLAGE}\t5\t1\t1\tok" in out
    assert out.index("Water\t2") < out.index("Flame\t1")
    assert "Stone" not in out


def test_leftovers_count_the_glbs_using_each_material():
    assert glb_audit.leftovers(floors_manifest()[VILLAGE]) == Counter(
        {"Water": 2, "Flame": 1, glb_audit.NO_MESH: 1}
    )


def test_a_missed_floor_fails(capsys):
    assert not glb_audit.report(floors_manifest(), {VILLAGE: 2})
    assert f"{VILLAGE}\t5\t1\t2\tFAIL" in capsys.readouterr().out


def test_a_pack_with_no_floor_and_a_floor_with_no_pack_fail(capsys):
    assert not glb_audit.report(floors_manifest(), {"Other/Pack": 1})
    out = capsys.readouterr().out
    assert f"{VILLAGE}\t5\t1\t-\tFAIL, no floor" in out
    assert "Other/Pack\t-\t-\t1\tFAIL, no such pack" in out


def test_compare_exits_1_on_a_missed_floor_and_0_once_it_holds(tmp_path):
    side = tmp_path / "side.tsv"
    glb_audit.write_manifest({VILLAGE: {"a.glb": glb()}}, side)
    pack_map = tsv(
        tmp_path / "map.tsv",
        "\t".join(glb_audit.PACK_MAP_COLUMNS),
        f"{VILLAGE}\t\t{VILLAGE}\t\t1",
    )
    args = ["compare", str(side), str(side), "--pack-map", str(pack_map), "--floors"]
    assert (
        glb_audit.main([*args, str(tsv(tmp_path / "f.tsv", "pack\tfloor", f"{VILLAGE}\t2"))]) == 1
    )
    assert (
        glb_audit.main([*args, str(tsv(tmp_path / "f.tsv", "pack\tfloor", f"{VILLAGE}\t1"))]) == 0
    )


def test_the_committed_manifest_gives_the_plans_now_column():
    manifest = glb_audit.read_manifest(PLAN / "baseline-v4.tsv")
    assert {pack: glb_audit.pack_counts(glbs) for pack, glbs in manifest.items()} == NOW
    medieval = manifest[MEDIEVAL]
    assert sum(path.startswith(URP) for path in medieval) == 193
    assert sum(path.startswith(BUILT_IN) for path in medieval) == 193


# The plan's "From Piece 2 on" table. A later ticket fills in Medieval Houses, and its row here.
RENAMED = {
    "Stylarts/StylizedVillage": "Stylarts/Stylized Village",
    "Leartes Studios/AncientRuins": "Leartes Studios/Ancient Ruins",
    "Stylarts/SpookyCemeteryCrypt": "Stylarts/Spooky Cemetery Crypt",
    "Stylarts/StylizedElvenValley": "Stylarts/Stylized Elven Valley",
    "Stylarts/StylizedLostCave": "Stylarts/Stylized Lost Cave",
    "Stylarts/StylizedWizardRoom": "Stylarts/Stylized Wizard Room",
}


# The GLBs each pack must match: Medieval Houses only its URP copy's, from Piece 2 on.
MATCHED = {**{p: n for p, (n, _) in NOW.items()}, MEDIEVAL: 193}


def test_the_committed_map_names_the_fab_packs_as_the_plan_does():
    pack_map = glb_audit.read_pack_map(PLAN / "pack-map.tsv")
    assert {r.baseline_pack: r.glbs for r in pack_map} == MATCHED
    for r in pack_map:
        assert r.current_pack == RENAMED.get(r.baseline_pack, r.baseline_pack)
        assert r.baseline_prefix == (URP if r.baseline_pack == MEDIEVAL else "")
        assert r.current_prefix == ""


def test_the_committed_map_matches_every_glb_under_the_new_names_and_only_there():
    baseline = glb_audit.read_manifest(PLAN / "baseline-v4.tsv")
    pack_map = glb_audit.read_pack_map(PLAN / "pack-map.tsv")

    current = {RENAMED.get(pack, pack): glbs for pack, glbs in baseline.items()}
    # Medieval Houses lands one pipeline, and `flatten` takes its folder away.
    current[MEDIEVAL] = {
        path.removeprefix(URP): glb for path, glb in baseline[MEDIEVAL].items() if URP in path
    }
    result = glb_audit.compare(baseline, current, pack_map)
    assert_passed(result)
    assert result.matched == MATCHED
    assert result.expected_absent == {f"{MEDIEVAL}\tMedieval_Houses_Vol1_Built-In/": 193}
    assert not result.geometry

    # A pack still at its old name fails loudly, so the map can't pass by comparing nothing.
    stale = glb_audit.compare(baseline, {**baseline, MEDIEVAL: current[MEDIEVAL]}, pack_map)
    assert len(stale.short_packs) == len(RENAMED)
    assert len(stale.unmatched_baseline) == sum(NOW[p][0] for p in RENAMED)
    assert len(stale.unmatched_current) == sum(NOW[p][0] for p in RENAMED)


PACK_MAP = (
    "\t".join(glb_audit.PACK_MAP_COLUMNS),
    f"{MEDIEVAL}\t{URP}\tGameAssetFactory/Medieval\tHouses/\t2",
)


def png(path: Path, color: tuple[int, int, int, int], size: tuple[int, int] = (64, 32)) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    Image.new("RGBA", size, color).save(path)


def sheet_library(root: Path, rows: dict[str, str | None]) -> Path:
    make_library(root, [])
    with sqlite3.connect(root / ".opendam" / "library.sqlite") as conn:
        conn.executemany("INSERT INTO assets VALUES (?, ?)", rows.items())
    conn.close()
    return root


def sheet_args(library: Path, tmp_path: Path, side: str, *paths: str) -> list[str]:
    return [
        str(library),
        "--pack",
        MEDIEVAL,
        "--paths",
        str(tsv(tmp_path / "paths.txt", "# one pack's models", "", *paths)),
        "--side",
        side,
        "--pack-map",
        str(tsv(tmp_path / "map.tsv", *PACK_MAP)),
        "--out",
        str(tmp_path / "sheet.png"),
        "--tile",
        "64",
        "--columns",
        "2",
    ]


def test_a_sheet_finds_each_side_through_the_map(tmp_path):
    before = sheet_library(
        tmp_path / "before", {f"{MEDIEVAL}/{URP}a.glb": ".opendam/thumbs/a_thumb.png"}
    )
    after = sheet_library(
        tmp_path / "after", {"GameAssetFactory/Medieval/Houses/a.glb": ".opendam/thumbs/a.png"}
    )
    png(before / ".opendam/thumbs/a_thumb.png", (255, 0, 0, 255))
    png(after / ".opendam/thumbs/a.png", (0, 255, 0, 255))

    for library, side, color in (
        (before, "baseline", (255, 0, 0)),
        (after, "current", (0, 255, 0)),
    ):
        assert contact_sheet.main(sheet_args(library, tmp_path, side, f"{URP}a.glb")) == 0
        with Image.open(tmp_path / "sheet.png") as sheet:
            x = contact_sheet.GAP + 32
            assert sheet.convert("RGB").getpixel((x, contact_sheet.TITLE_HEIGHT + 32)) == color


def test_a_list_path_outside_the_baseline_prefix_stops_the_sheet(tmp_path, capsys):
    library = sheet_library(tmp_path / "lib", {f"{MEDIEVAL}/{BUILT_IN}a.glb": None})
    args = sheet_args(library, tmp_path, "baseline", f"{BUILT_IN}a.glb")
    assert contact_sheet.main(args) == 2
    assert "outside the baseline prefix" in capsys.readouterr().err
    assert not (tmp_path / "sheet.png").exists()


def test_a_path_the_library_doesnt_hold_stops_the_sheet(tmp_path, capsys):
    library = sheet_library(tmp_path / "lib", {})
    assert contact_sheet.main(sheet_args(library, tmp_path, "baseline", f"{URP}a.glb")) == 2
    assert "doesn't hold" in capsys.readouterr().err
    assert not (tmp_path / "sheet.png").exists()


def test_a_baseline_pack_with_no_map_row_stops_the_sheet(tmp_path, capsys):
    library = sheet_library(tmp_path / "lib", {})
    args = sheet_args(library, tmp_path, "baseline", f"{URP}a.glb")
    args[args.index("--pack") + 1] = "Other/Pack"
    assert contact_sheet.main(args) == 2
    assert "no pack map row" in capsys.readouterr().err


def test_a_model_with_no_png_gets_a_marked_tile_beside_the_rest(tmp_path, capsys):
    library = sheet_library(
        tmp_path / "lib",
        {
            f"{MEDIEVAL}/{URP}a.glb": ".opendam/thumbs/a_thumb.png",
            f"{MEDIEVAL}/{URP}b.glb": ".opendam/thumbs/b_preview.glb",
        },
    )
    png(library / ".opendam/thumbs/a_thumb.png", (255, 0, 0, 255))
    (library / ".opendam/thumbs/b_preview.glb").write_bytes(b"glTF")

    args = sheet_args(library, tmp_path, "baseline", f"{URP}a.glb", f"{URP}b.glb")
    assert contact_sheet.main(args) == 0
    assert f"no PNG thumbnail: {MEDIEVAL}/{URP}b.glb" in capsys.readouterr().err


def test_a_sheet_with_no_thumbnail_at_all_exits_1(tmp_path):
    library = sheet_library(tmp_path / "lib", {f"{MEDIEVAL}/{URP}a.glb": None})
    assert contact_sheet.main(sheet_args(library, tmp_path, "baseline", f"{URP}a.glb")) == 1


def test_a_sheet_refuses_to_write_inside_the_library_and_changes_nothing(tmp_path):
    library = sheet_library(tmp_path / "lib", {f"{MEDIEVAL}/{URP}a.glb": "t.png"})
    png(library / "t.png", (255, 0, 0, 255))
    before = snapshot(library)

    args = sheet_args(library, tmp_path, "baseline", f"{URP}a.glb")
    assert contact_sheet.main(args) == 0
    args[args.index("--out") + 1] = str(library / "sheet.png")
    assert contact_sheet.main(args) == 2

    assert snapshot(library) == before
