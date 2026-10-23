"""The game export's `fbx_facts` (plan game-export C12) and the node-tree reader it uses.

The Blender-made fixtures under the export's fixture library are read as committed. Binary
FBX 7.5 and ASCII files, which Blender doesn't write, are built here from one scene.

`fbx_facts.json`, beside the fixture library, holds the facts of its FBX files, which stand in
for the worker in the Rust Check's tests. After `make_fixtures.py` changes them, rewrite it with
`PYTHONPATH=. uv run --frozen python tests/test_fbx_facts.py`, then format it with `bun run format`
at the repository root, as `make lint` checks.
"""

from __future__ import annotations

import builtins
import json
import struct
from dataclasses import dataclass, field
from pathlib import Path

import pytest

import fbx_nodes
import worker

LIBRARY = (
    Path(__file__).resolve().parents[3]
    / "apps/desktop/src-tauri/tests/fixtures/game-export/library"
)
MODELS = LIBRARY / "Dungeon Props/Models"
SNAPSHOT = LIBRARY.parent / "fbx_facts.json"

# What the app sends: `export::collision::PATTERNS`
PATTERNS = {"prefixes": ["UCX_", "UBX_", "USP_", "UCP_"], "suffixes": ["_Collision", "_COL"]}


def facts(name: str, patterns: dict = PATTERNS) -> dict:
    result = worker.fbx_facts(str(MODELS / name), patterns, str(LIBRARY))
    assert result["ok"], result
    return result


def slot_names(result: dict) -> list[str]:
    return [slot["name"] for slot in result["slots"]]


def test_an_unreal_style_model_keeps_its_lods_and_drops_its_hull_with_its_material():
    result = facts("SM_Pillar.fbx")
    assert slot_names(result) == ["Stone", "Moss"]
    assert result["kept_meshes"] == ["SM_Pillar_LOD0", "SM_Pillar_LOD1"]
    assert result["dropped_meshes"] == ["UCX_SM_Pillar_00"]
    assert result["has_kept_mesh"] and not result["kept_mesh_without_material"]
    assert result["bones"] == [] and result["takes"] == []


def test_texture_references_are_reported_by_type_as_the_fbx_gives_them():
    stone = facts("SM_Pillar.fbx")["slots"][0]
    assert sorted(stone["textures"]) == ["baseColor", "normal"]
    [base] = stone["textures"]["baseColor"]
    [normal] = stone["textures"]["normal"]
    assert base["relative"] == "../Textures/T_StoneFloor_D.png"
    assert normal["relative"] == "../Textures/T_StoneFloor_N.png"
    # The absolute path of the machine that made it, as real FBX files hold
    assert base["file"].endswith("/Dungeon Props/Textures/T_StoneFloor_D.png")
    assert facts("SM_Pillar.fbx")["slots"][1]["textures"] == {}


def test_a_hull_without_a_material_is_dropped_too():
    result = facts("SM_Gate.fbx")
    assert slot_names(result) == ["Iron"]
    assert result["kept_meshes"] == ["SM_Gate_LOD0", "SM_Gate_LOD1"]
    assert result["dropped_meshes"] == ["UCX_SM_Gate_00"]
    assert not result["kept_mesh_without_material"]


def test_a_model_with_no_material_has_no_slot_and_a_mesh_without_one():
    result = facts("SM_Boulder.fbx")
    assert result["slots"] == []
    assert result["has_kept_mesh"] and result["kept_mesh_without_material"]


def test_a_skinned_model_lists_its_bones_and_its_take():
    result = facts("SK_Guard.fbx")
    assert slot_names(result) == ["Cloth"]
    assert result["kept_meshes"] == ["SK_Guard"]
    assert result["bones"] == ["Hips", "Spine", "Head", "Head_end"]
    assert result["takes"] == ["Wave"]


@pytest.mark.parametrize(
    ("name", "dropped", "bones", "takes"),
    [
        # No mesh at all
        ("A_GuardIdle.fbx", [], ["Hips", "Spine", "Head", "Head_end"], ["Idle"]),
        # Its only mesh is a hull, which the export drops
        ("A_GuardNod.fbx", ["UCX_A_GuardNod_00"], ["Hips", "Spine", "Head", "Head_end"], ["Nod"]),
        ("A_Wag.fbx", [], ["Hips", "Tail", "Tail_end"], ["Wag"]),
    ],
)
def test_an_animation_has_takes_and_bones_and_no_kept_mesh(name, dropped, bones, takes):
    result = worker.fbx_facts(
        str(LIBRARY / "Dungeon Props/Animations" / name), PATTERNS, str(LIBRARY)
    )
    assert result["ok"], result
    assert result["kept_meshes"] == [] and not result["has_kept_mesh"]
    assert result["dropped_meshes"] == dropped
    assert result["slots"] == []
    assert result["bones"] == bones
    assert result["takes"] == takes


def test_an_fbx_holding_only_a_hull_has_no_kept_mesh_slot_or_take():
    result = facts("SM_Hull.fbx")
    assert result["kept_meshes"] == [] and not result["has_kept_mesh"]
    assert result["dropped_meshes"] == ["UCX_SM_Hull_00"]
    assert result["slots"] == [] and result["takes"] == []


@pytest.mark.parametrize(
    ("name", "slots", "without_material"),
    [
        ("SM_Stool.fbx", ["Material"], False),
        ("SM_Bench.fbx", ["Wood", "Wood"], False),
        # A real DefaultMaterial is kept, and no added one is listed for the other mesh
        ("SM_Lamp.fbx", ["DefaultMaterial"], True),
        ("SM_Shelf.fbx", ["OpenDAMDefault"], True),
    ],
)
def test_only_declared_materials_are_slots_duplicates_included(name, slots, without_material):
    result = facts(name)
    assert slot_names(result) == slots
    assert result["kept_mesh_without_material"] is without_material


def test_the_patterns_sent_decide_what_is_dropped_ignoring_case():
    kept = facts("SM_Pillar.fbx", {"prefixes": [], "suffixes": []})
    assert kept["kept_meshes"] == ["SM_Pillar_LOD0", "SM_Pillar_LOD1", "UCX_SM_Pillar_00"]
    assert slot_names(kept) == ["Stone", "Moss", "Collision"]
    lod1 = facts("SM_Pillar.fbx", {"prefixes": ["ucx_"], "suffixes": ["_lod1"]})
    assert lod1["dropped_meshes"] == ["SM_Pillar_LOD1", "UCX_SM_Pillar_00"]


def test_it_reads_the_fbx_and_no_texture_file(monkeypatch):
    opened = []
    real_open = builtins.open

    def recording_open(file, *args, **kwargs):
        opened.append(Path(file).name)
        return real_open(file, *args, **kwargs)

    monkeypatch.setattr(builtins, "open", recording_open)
    facts("SM_Totem.fbx")
    assert opened == ["SM_Totem.fbx"]


def test_an_fbx_outside_the_library_root_is_not_read(tmp_path, monkeypatch):
    library = tmp_path / "library"
    library.mkdir()
    outside = tmp_path / "outside.fbx"
    outside.write_bytes((MODELS / "SM_Stool.fbx").read_bytes())
    (library / "link.fbx").symlink_to(outside)
    opened = []
    monkeypatch.setattr(builtins, "open", lambda *a, **k: opened.append(a) or 1 / 0)

    for source in [outside, library / "link.fbx"]:
        result = worker.fbx_facts(str(source), PATTERNS, str(library))
        assert result["ok"] is False and "outside the library root" in result["error"]
    result = worker.fbx_facts(str(MODELS / "SM_Stool.fbx"), PATTERNS, None)
    assert result == {"ok": False, "error": "no library root was given"}
    assert opened == []


def test_a_file_that_isnt_an_fbx_or_is_cut_short_is_an_error(tmp_path):
    text = tmp_path / "notes.fbx"
    text.write_text("not a model\n")
    cut = tmp_path / "cut.fbx"
    cut.write_bytes((MODELS / "SM_Pillar.fbx").read_bytes()[:400])
    for source in [text, cut]:
        result = worker.fbx_facts(str(source), PATTERNS, str(tmp_path))
        assert result["ok"] is False and result["error"].startswith("can't read the FBX: ")


def snapshot() -> dict:
    """The facts of every FBX in the fixture library, by path relative to it, without `ok`."""
    facts = {}
    for path in sorted(LIBRARY.rglob("*.fbx")):
        result = worker.fbx_facts(str(path), PATTERNS, str(LIBRARY))
        assert result.pop("ok"), result
        facts[path.relative_to(LIBRARY).as_posix()] = result
    return facts


def test_the_rust_checks_facts_snapshot_is_what_the_worker_reads():
    assert json.loads(SNAPSHOT.read_text()) == snapshot()


def test_the_worker_dispatches_fbx_facts():
    request = {"source": str(MODELS / "SM_Stool.fbx"), "patterns": PATTERNS}
    result = worker.DISPATCH["fbx_facts"]({**request, "library_root": str(LIBRARY)})
    assert slot_names(result) == ["Material"]
    json.dumps(result)


# ── Binary FBX 7.5 and ASCII, built from one scene ────────────────────────
@dataclass
class N:
    name: str
    props: list = field(default_factory=list)
    children: list = field(default_factory=list)


def _bprop(p) -> bytes:
    if isinstance(p, fbx_nodes.Array):
        return b"i" + struct.pack("<III", p.count, 0, p.count * 4) + b"\0" * (p.count * 4)
    if isinstance(p, int):
        return b"L" + struct.pack("<q", p)
    if isinstance(p, float):
        return b"D" + struct.pack("<d", p)
    data = p.encode()
    return b"S" + struct.pack("<I", len(data)) + data


def _bnode(node: N, offset: int, wide: bool) -> bytes:
    head = struct.Struct("<QQQB" if wide else "<IIIB")
    name = node.name.encode()
    props = b"".join(_bprop(p) for p in node.props)
    body = b""
    start = offset + head.size + len(name) + len(props)
    for child in node.children:
        body += _bnode(child, start + len(body), wide)
    if node.children:
        body += b"\0" * head.size
    return (
        head.pack(start + len(body), len(node.props), len(props), len(name)) + name + props + body
    )


def binary_fbx(nodes: list[N], version: int) -> bytes:
    data = fbx_nodes.BINARY_MAGIC + b"\x1a\x00" + struct.pack("<I", version)
    for node in nodes:
        data += _bnode(node, len(data), version >= 7500)
    return data + b"\0" * (25 if version >= 7500 else 13)


def _aprop(p) -> str:
    if isinstance(p, fbx_nodes.Array):
        return f"*{p.count}"
    return json.dumps(p) if isinstance(p, str) else str(p)


def _anode(node: N, indent: int) -> str:
    pad = "\t" * indent
    line = f"{pad}{node.name}: {', '.join(_aprop(p) for p in node.props)}"
    if len(node.props) == 1 and isinstance(node.props[0], fbx_nodes.Array):
        count = node.props[0].count
        return f"{line} {{\n{pad}\ta: {','.join(['0'] * count)}\n{pad}}}\n"
    if not node.children:
        return line + "\n"
    inner = "".join(_anode(c, indent + 1) for c in node.children)
    return f"{line} {{\n{inner}{pad}}}\n"


def ascii_fbx(nodes: list[N]) -> str:
    text = "; FBX 7.4.0 project file\n; ----------------------------------------------------\n\n"
    for node in nodes:
        text += _anode(node, 0) + "\n"
        if node.name == "Connections":
            # ASCII files comment every connection
            text = text.replace('C: "OO"', ';a comment\n\tC: "OO"', 1)
    return text


def name(cls: str, value: str, ascii: bool) -> str:
    return f"{cls}::{value}" if ascii else f"{value}\x00\x01{cls}"


def scene(ascii: bool) -> list[N]:
    """A rock with a material in two names, a collision mesh, two bones, a take that moves a
    bone and one whose curve has no key."""

    def obj(kind, oid, cls, value, sub, children=()):
        return N(kind, [oid, name(cls, value, ascii), sub], list(children))

    def keys(count):
        return [N("Default", [0.0]), N("KeyTime", [fbx_nodes.Array(count)])]

    objects = [
        obj("Geometry", 1, "Geometry", "Rock", "Mesh", [N("Vertices", [fbx_nodes.Array(9)])]),
        obj("Model", 10, "Model", "Rock", "Mesh", [N("Version", [232])]),
        obj("Model", 11, "Model", "Rock_Collision", "Mesh"),
        obj("Model", 12, "Model", "Root", "LimbNode"),
        obj("Model", 13, "Model", "Tip", "LimbNode"),
        obj("Material", 20, "Material", "Granite", ""),
        obj("Material", 21, "Material", "Hull", ""),
        obj(
            "Texture",
            30,
            "Texture",
            "granite",
            "",
            [N("FileName", ["C:/art/granite.png"]), N("RelativeFilename", ["granite.png"])],
        ),
        obj("AnimationStack", 40, "AnimStack", "Idle", ""),
        obj("AnimationLayer", 41, "AnimLayer", "Base", ""),
        obj("AnimationCurveNode", 42, "AnimCurveNode", "R", ""),
        obj("AnimationCurve", 43, "AnimCurve", "", "", keys(3)),
        obj("AnimationStack", 50, "AnimStack", "Still", ""),
        obj("AnimationLayer", 51, "AnimLayer", "Base", ""),
        obj("AnimationCurveNode", 52, "AnimCurveNode", "R", ""),
        obj("AnimationCurve", 53, "AnimCurve", "", "", keys(0)),
    ]
    connections = [
        ["OO", 10, 0],
        ["OO", 11, 0],
        ["OO", 1, 10],
        ["OO", 12, 0],
        ["OO", 13, 12],
        ["OO", 20, 10],
        ["OO", 21, 11],
        ["OP", 30, 20, "DiffuseColor"],
        ["OO", 41, 40],
        ["OO", 42, 41],
        ["OP", 42, 13, "Lcl Rotation"],
        ["OP", 43, 42, "d|X"],
        ["OO", 51, 50],
        ["OO", 52, 51],
        ["OP", 52, 13, "Lcl Rotation"],
        ["OP", 53, 52, "d|X"],
    ]
    return [
        N("FBXHeaderExtension", [], [N("FBXVersion", [7400])]),
        N("Definitions", [], [N("Version", [100])]),
        N("Objects", [], objects),
        N("Connections", [], [N("C", c) for c in connections]),
    ]


EXPECTED = {
    "ok": True,
    "slots": [
        {
            "name": "Granite",
            "textures": {"baseColor": [{"file": "C:/art/granite.png", "relative": "granite.png"}]},
        }
    ],
    "kept_meshes": ["Rock"],
    "dropped_meshes": ["Rock_Collision"],
    "has_kept_mesh": True,
    "kept_mesh_without_material": False,
    "bones": ["Root", "Tip"],
    "takes": ["Idle"],
}


@pytest.mark.parametrize("version", [7400, 7500, 7700])
def test_binary_files_read_with_32_and_64_bit_records(tmp_path, version):
    source = tmp_path / "rock.fbx"
    source.write_bytes(binary_fbx(scene(ascii=False), version))
    assert fbx_nodes.read(source).version == version
    assert worker.fbx_facts(str(source), PATTERNS, str(tmp_path)) == EXPECTED


def test_ascii_files_give_the_same_facts(tmp_path):
    source = tmp_path / "rock.fbx"
    source.write_text(ascii_fbx(scene(ascii=True)))
    doc = fbx_nodes.read(source)
    assert doc.version == 7400
    assert sorted(doc.sections) == ["Connections", "FBXHeaderExtension", "Objects"]
    geometry = doc.sections["Objects"].children[0]
    assert (geometry.name, geometry.children) == ("Geometry", [])
    assert worker.fbx_facts(str(source), PATTERNS, str(tmp_path)) == EXPECTED


def test_an_fbx_older_than_7_is_refused(tmp_path):
    source = tmp_path / "old.fbx"
    source.write_bytes(binary_fbx(scene(ascii=False), 6100))
    result = worker.fbx_facts(str(source), PATTERNS, str(tmp_path))
    assert result == {"ok": False, "error": "FBX version 6100 is older than FBX 7"}


if __name__ == "__main__":
    SNAPSHOT.write_text(json.dumps(snapshot(), indent=2) + "\n")
