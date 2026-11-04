"""Writes the Unity GLB converter's fixture packs (docs/plans/import-material-fixes/plan.md,
Fixture tests) beside this script, replacing what is there:

    python3 -I make_fixtures.py

It uses only the Python standard library and fixes every GUID, so a rerun gives the same bytes.
Its output is committed, like `tests/fixtures/pack-import/`. Real Asset Store content can't be.

Each folder is one pack, so a case's by-name lookups reach only its own files. Every pack holds
`Model.obj`, one triangle per material slot, and `Model.mtl`, which name the slots through `usemtl`
and `newmtl`, with text `.mat`, `.meta` and `.shadergraph` files and solid 4 x 4 PNGs. A pack about
which meshes stay holds one triangle per named object instead, in a model of its own name when
the file name is the case (`Pack.objects`). `mesh-name` and `exact-first` hold no model: an OBJ's
object and mesh share a name, so their tests make `Model.fbx` under Blender. The tests in
`src/import/unity_glb_fixture_tests.rs` tell a texture by its colour:

- BASE, OTHER_BASE, NORMAL, ORM and MASK are solid colours: only a role's right texture is BASE or
  NORMAL, and MASK is black, so a mask read as a base shows.
- An alpha base is BASE with an alpha of 0. Its red is high, so a clip that read its red channel
  as a mask keeps what its own alpha cuts.
- TINT is the flat colour a material should show, as Unity stores it, in sRGB, and GREY one it
  should not.
"""

import hashlib
import json
import shutil
import struct
import zlib
from pathlib import Path

HERE = Path(__file__).resolve().parent

BASE = (200, 40, 40)
OTHER_BASE = (40, 200, 40)
NORMAL = (128, 128, 255)
ORM = (10, 120, 220)
MASK = (0, 0, 0)
TINT = (0.2, 0.4, 0.6, 1)
GREY = (0.8, 0.8, 0.8, 1)

# The GUID of Unity's built-in resources, which a material on a built-in shader names.
BUILT_IN = "0000000000000000f000000000000000"
# The file ID every Shader Graph's main asset has in a material's `m_Shader`, and a `.shader`'s.
GRAPH_FILE_ID = -6465566751694194690
SHADER_FILE_ID = 4800000


def guid(pack: str, name: str) -> str:
    return hashlib.md5(f"{pack}/{name}".encode()).hexdigest()


def png(rgb: tuple[int, int, int], alpha: int | None = None) -> bytes:
    """A 4 x 4 PNG of one colour, with an alpha channel when `alpha` is given."""

    def chunk(kind: bytes, data: bytes) -> bytes:
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    pixel = bytes(rgb) if alpha is None else bytes(rgb) + bytes([alpha])
    rows = (b"\0" + pixel * 4) * 4
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", 4, 4, 8, 2 if alpha is None else 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(rows, 9))
        + chunk(b"IEND", b"")
    )


class Pack:
    def __init__(self, name: str, slots: list[str] | None = None):
        self.name = name
        self.dir = HERE / name
        self.dir.mkdir()
        if slots:
            self.mtl(slots)

    def write(self, name: str, text: str | bytes) -> None:
        data = text.encode() if isinstance(text, str) else text
        (self.dir / name).write_bytes(data)

    def meta(self, name: str) -> None:
        self.write(f"{name}.meta", f"fileFormatVersion: 2\nguid: {guid(self.name, name)}\n")

    def mtl(self, slots: list[str]) -> None:
        """The model, one triangle per slot, and a `.mtl` naming each."""
        obj = ["mtllib Model.mtl", "o Model"]
        for n, slot in enumerate(slots):
            at = n * 3
            obj += [f"v {at} 0 0", f"v {at + 1} 0 0", f"v {at} 1 0"]
            obj += ["vt 0 0", "vt 1 0", "vt 0 1"]
            obj += [f"usemtl {slot}", f"f {at + 1}/{at + 1} {at + 2}/{at + 2} {at + 3}/{at + 3}"]
        self.write("Model.obj", "\n".join(obj) + "\n")
        self.write("Model.mtl", "".join(f"newmtl {slot}\nKd 1 1 1\n" for slot in slots))

    def objects(self, names: list[str], model: str = "Model") -> None:
        """`model`.obj, one triangle per object of `names`, all on one slot, and its `.mtl`."""
        obj = [f"mtllib {model}.mtl"]
        for n, name in enumerate(names):
            at = n * 3
            obj += [f"o {name}", f"v {at} 0 0", f"v {at + 1} 0 0", f"v {at} 1 0"]
            obj += ["vt 0 0", "vt 1 0", "vt 0 1"]
            obj += ["usemtl Imported", f"f {at + 1}/{at + 1} {at + 2}/{at + 2} {at + 3}/{at + 3}"]
        self.write(f"{model}.obj", "\n".join(obj) + "\n")
        self.write(f"{model}.mtl", "newmtl Imported\nKd 1 1 1\n")

    def texture(self, name: str, rgb: tuple[int, int, int], alpha: int | None = None) -> str:
        """Writes `name`.png and its `.meta`, and returns its GUID."""
        self.write(f"{name}.png", png(rgb, alpha))
        self.meta(f"{name}.png")
        return guid(self.name, f"{name}.png")

    def graph(self, name: str, text: str) -> str:
        """Writes `name`.shadergraph and its `.meta`, and returns its GUID."""
        self.write(f"{name}.shadergraph", text)
        self.meta(f"{name}.shadergraph")
        return guid(self.name, f"{name}.shadergraph")

    def shader(self, name: str, text: str) -> str:
        """Writes `name`.shader and its `.meta`, and returns its GUID."""
        self.write(f"{name}.shader", text)
        self.meta(f"{name}.shader")
        return guid(self.name, f"{name}.shader")

    def material(
        self,
        name: str,
        textures=None,
        colors=None,
        shader=None,
        cutout=False,
        file_id=GRAPH_FILE_ID,
    ) -> None:
        """Writes `name`.mat and its `.meta`, a material as Unity 2021 writes it: `textures`
        maps a property to a texture's GUID, or `None` for an empty slot, and `shader` is the
        GUID of a graph, or with `SHADER_FILE_ID`, a `.shader`."""
        lines = [
            "%YAML 1.1",
            "%TAG !u! tag:unity3d.com,2011:",
            "--- !u!21 &2100000",
            "Material:",
            "  serializedVersion: 8",
            f"  m_Name: {name}",
        ]
        if shader:
            # Unity wraps the line here, past 80 columns.
            lines += [f"  m_Shader: {{fileID: {file_id}, guid: {shader},", "    type: 3}"]
        else:
            lines.append(f"  m_Shader: {{fileID: 46, guid: {BUILT_IN}, type: 0}}")
        if cutout:
            lines.append("  m_ShaderKeywords: _ALPHATEST_ON")
        lines += ["  m_SavedProperties:", "    serializedVersion: 3", "    m_TexEnvs:"]
        for prop, bound in (textures or {}).items():
            target = f"{{fileID: 2800000, guid: {bound}, type: 3}}" if bound else "{fileID: 0}"
            lines += [
                f"    - {prop}:",
                f"        m_Texture: {target}",
                "        m_Scale: {x: 1, y: 1}",
                "        m_Offset: {x: 0, y: 0}",
            ]
        lines += ["    m_Ints: []", "    m_Floats: []", "    m_Colors:"]
        for prop, (r, g, b, a) in (colors or {}).items():
            lines.append(f"    - {prop}: {{r: {r}, g: {g}, b: {b}, a: {a}}}")
        self.write(f"{name}.mat", "\n".join(lines) + "\n")
        self.meta(f"{name}.mat")


def stream(
    properties: dict[str, str],
    override: dict[str, str] | None = None,
    hdr: tuple[str, ...] = (),
) -> str:
    """A Shader Graph as Unity 2021 writes it: a stream of JSON objects, the graph, then one for
    each property of `properties`, reference -> display name, a color for a generated `Color_`
    reference and a texture otherwise. `override` holds the references set by hand: the generated
    one stays in `m_DefaultReferenceName`. `hdr` holds the colors whose mode is HDR."""
    override = override or {}

    def kind(ref: str) -> str:
        return "Color" if ref.startswith("Color_") else "Texture2D"

    objects = [
        {
            "m_SGVersion": 3,
            "m_Type": "UnityEditor.ShaderGraph.GraphData",
            "m_Properties": [{"m_Id": ref} for ref in properties],
        },
        # A node has an `m_Name` too, which isn't a property's
        {"m_Type": "UnityEditor.ShaderGraph.BlockNode", "m_Name": "SurfaceDescription.BaseColor"},
    ]
    for ref, shown in properties.items():
        found = {
            "m_Type": f"UnityEditor.ShaderGraph.Internal.{kind(ref)}ShaderProperty",
            "m_Name": shown,
            "m_DefaultReferenceName": ref,
            "m_OverrideReferenceName": override.get(ref, ""),
        }
        if kind(ref) == "Color":
            found["m_ColorMode"] = 1 if ref in hdr else 0
        objects.append(found)
    return "\n\n".join(json.dumps(o, indent=4) for o in objects) + "\n"


def older(properties: dict[str, str]) -> str:
    """A graph from before the stream: one object, its properties as escaped JSON text."""
    return json.dumps(
        {
            "m_SerializedProperties": [
                {
                    "typeInfo": {"fullName": "UnityEditor.ShaderGraph.TextureShaderProperty"},
                    "JSONnodeData": json.dumps({"m_Name": shown, "m_DefaultReferenceName": ref}),
                }
                for ref, shown in properties.items()
            ]
        },
        indent=4,
    )


def clip_stream(
    prop: str, alpha_from: int = 7, clip: bool = True, override: bool = False, shown: str = "Base"
) -> str:
    """A Shader Graph as Unity 2021 writes it whose Alpha block reads output `alpha_from` of a
    sample of the texture property `prop`, shown as `shown` (7 is its alpha, 4 its red), and
    whose target clips when `clip` is set, or leaves that to the material with `override`."""

    def edge(out_node: str, out_slot: int, in_node: str, in_slot: int) -> dict:
        return {
            "m_OutputSlot": {"m_Node": {"m_Id": out_node}, "m_SlotId": out_slot},
            "m_InputSlot": {"m_Node": {"m_Id": in_node}, "m_SlotId": in_slot},
        }

    objects = [
        {
            "m_SGVersion": 3,
            "m_Type": "UnityEditor.ShaderGraph.GraphData",
            "m_ObjectId": "graph",
            "m_Properties": [{"m_Id": "base"}],
            "m_Edges": [edge("property", 0, "sample", 1), edge("sample", alpha_from, "alpha", 0)],
        },
        {
            "m_Type": "UnityEditor.ShaderGraph.Internal.Texture2DShaderProperty",
            "m_ObjectId": "base",
            "m_Name": shown,
            "m_DefaultReferenceName": "Texture2D_x",
            "m_OverrideReferenceName": prop,
        },
        {
            "m_Type": "UnityEditor.ShaderGraph.PropertyNode",
            "m_ObjectId": "property",
            "m_Property": {"m_Id": "base"},
        },
        {"m_Type": "UnityEditor.ShaderGraph.SampleTexture2DNode", "m_ObjectId": "sample"},
        {
            "m_Type": "UnityEditor.ShaderGraph.BlockNode",
            "m_ObjectId": "alpha",
            "m_Name": "SurfaceDescription.Alpha",
        },
        {
            "m_Type": "UnityEditor.Rendering.Universal.ShaderGraph.UniversalTarget",
            "m_ObjectId": "target",
            "m_AlphaClip": clip,
            "m_AllowMaterialOverride": override,
        },
    ]
    return "\n\n".join(json.dumps(o, indent=4) for o in objects) + "\n"


def older_clip(prop: str, threshold: float, linked: bool = False) -> str:
    """A graph from before the stream: a PBR master node whose Alpha reads the alpha of a sample
    of the texture property `prop`, and whose AlphaClipThreshold is `threshold`. With `linked`,
    it has `FAE_TreeBranch`'s shape: the threshold is linked to a `_Cutoff` property, and the
    sample's alpha reaches Alpha through a sub graph (its `CrossfadeLOD`)."""

    def entry(kind: str, data: dict) -> dict:
        return {
            "typeInfo": {"fullName": f"UnityEditor.ShaderGraph.{kind}"},
            "JSONnodeData": json.dumps(data),
        }

    def edge(out_node: str, out_slot: int, in_node: str, in_slot: int) -> dict:
        return entry(
            "Graphing.Edge",
            {
                "m_OutputSlot": {"m_SlotId": out_slot, "m_NodeGUIDSerialized": out_node},
                "m_InputSlot": {"m_SlotId": in_slot, "m_NodeGUIDSerialized": in_node},
            },
        )

    slots = [
        entry("Vector1MaterialSlot", {"m_Id": 7, "m_DisplayName": "Alpha", "m_Value": 1.0}),
        entry(
            "Vector1MaterialSlot",
            {"m_Id": 8, "m_DisplayName": "AlphaClipThreshold", "m_Value": threshold},
        ),
    ]
    graph = {
        "m_SerializedProperties": [
            entry(
                "TextureShaderProperty",
                {
                    "m_Guid": {"m_GuidSerialized": "base"},
                    "m_Name": "MainTex",
                    "m_DefaultReferenceName": "Texture2D_x",
                    "m_OverrideReferenceName": prop,
                },
            )
        ],
        "m_SerializableNodes": [
            entry("PBRMasterNode", {"m_GuidSerialized": "master", "m_SerializableSlots": slots}),
            entry(
                "PropertyNode", {"m_GuidSerialized": "property", "m_PropertyGuidSerialized": "base"}
            ),
            entry("SampleTexture2DNode", {"m_GuidSerialized": "sample"}),
        ],
        "m_SerializableEdges": [edge("property", 0, "sample", 1)],
        "m_ActiveOutputNodeGuidSerialized": "master",
    }
    if linked:
        graph["m_SerializedProperties"].append(
            entry(
                "Internal.Vector1ShaderProperty",
                {
                    "m_Guid": {"m_GuidSerialized": "cutoff"},
                    "m_Name": "Alpha clipping",
                    "m_DefaultReferenceName": "Vector1_x",
                    "m_OverrideReferenceName": "_Cutoff",
                },
            )
        )
        graph["m_SerializableNodes"] += [
            entry(
                "PropertyNode", {"m_GuidSerialized": "clip", "m_PropertyGuidSerialized": "cutoff"}
            ),
            entry("SubGraphNode", {"m_GuidSerialized": "crossfade"}),
        ]
        graph["m_SerializableEdges"] += [
            edge("clip", 0, "master", 8),
            edge("sample", 7, "crossfade", 1),
            edge("crossfade", 2, "master", 7),
        ]
    else:
        graph["m_SerializableEdges"].append(edge("sample", 7, "master", 7))
    return json.dumps(graph, indent=4)


def shader_source(render_type: str) -> str:
    """A `.shader` whose SubShader's tags name `render_type`."""
    return (
        'Shader "Fixture/Thing" {\n'
        '    Properties { _MainTex ("Base", 2D) = "white" {} }\n'
        "    SubShader {\n"
        f'        Tags {{ "RenderType"="{render_type}" }}\n'
        "        CGPROGRAM\n"
        "        #pragma surface surf Standard\n"
        "        ENDCG\n"
        "    }\n"
        "}\n"
    )


def graph_material(pack: Pack, properties: dict[str, tuple[str, tuple]], colors=None) -> None:
    """`M_Thing`, whose graph shows each generated property, reference -> (display name, colour),
    under its display name, each bound to a texture of that colour."""
    shader = pack.graph("S_Thing", stream({ref: shown for ref, (shown, _) in properties.items()}))
    textures = {
        ref: pack.texture(f"T_{n}", rgb) for n, (ref, (_, rgb)) in enumerate(properties.items())
    }
    pack.material("M_Thing", textures, colors, shader)


def mapped_planes(pack: Pack, model: str, plain: bool = False) -> None:
    """Two meshes, `Plane.099` and `Plane.100`, mapped by those exact names in `model`'s `.meta`,
    and a prefab giving each its own material, `M_First` (BASE) and `M_Second` (OTHER_BASE). The
    lists differ, so no single list finds them, and only with `plain` does `Plane` without its
    suffix find anything: a third mesh, `M_Plain` (ORM), which an exact name must beat."""
    meshes = [(4300000, "Plane.099", "M_First"), (4300002, "Plane.100", "M_Second")]
    pack.material("M_First", {"_BaseMap": pack.texture("T_First", BASE)})
    pack.material("M_Second", {"_BaseMap": pack.texture("T_Second", OTHER_BASE)})
    if plain:
        meshes.append((4300004, "Plane", "M_Plain"))
        pack.material("M_Plain", {"_BaseMap": pack.texture("T_Plain", ORM)})
    pack.write(
        f"{model}.meta",
        "fileFormatVersion: 2\n"
        f"guid: {guid(pack.name, model)}\n"
        "ModelImporter:\n"
        "  fileIDToRecycleName:\n" + "".join(f"    {mesh}: {name}\n" for mesh, name, _ in meshes),
    )
    prefab = ["%YAML 1.1", "%TAG !u! tag:unity3d.com,2011:"]
    for n, (mesh, _, material) in enumerate(meshes):
        prefab += [
            f"--- !u!33 &{n}1",
            "MeshFilter:",
            f"  m_GameObject: {{fileID: {n}7}}",
            f"  m_Mesh: {{fileID: {mesh}, guid: {guid(pack.name, model)}, type: 3}}",
            f"--- !u!23 &{n}2",
            "MeshRenderer:",
            f"  m_GameObject: {{fileID: {n}7}}",
            "  m_Materials:",
            f"  - {{fileID: 2100000, guid: {guid(pack.name, material + '.mat')}, type: 2}}",
        ]
    pack.write("Model.prefab", "\n".join(prefab) + "\n")


def main() -> None:
    for old in HERE.iterdir():
        if old.is_dir():
            shutil.rmtree(old)

    pack = Pack("generated-names", ["M_Thing"])
    graph_material(
        pack,
        {
            "Texture2D_a": ("A_BaseColor", BASE),
            "Texture2D_b": ("NormalMap", NORMAL),
            "Texture2D_c": ("Material 01 ORM", ORM),
        },
        {"_Color": TINT},
    )

    pack = Pack("override-reference", ["M_Thing"])
    # Two textures, so the only-texture rule can't find the base without the override.
    shader = pack.graph(
        "S_Thing",
        stream(
            {"Texture2D_a": "A_BaseColor", "Texture2D_b": "Other"},
            {"Texture2D_a": "_Layer1", "Texture2D_b": "_Layer2"},
        ),
    )
    pack.material(
        "M_Thing",
        {"_Layer1": pack.texture("T_Base", BASE), "_Layer2": pack.texture("T_Other", OTHER_BASE)},
        shader=shader,
    )

    # B comes first in the material and in the graph.
    pack = Pack("layers", ["M_Thing"])
    graph_material(
        pack,
        {"Texture2D_a": ("B_BaseColor", OTHER_BASE), "Texture2D_b": ("A_BaseColor", BASE)},
    )

    pack = Pack("orm-word", ["M_Thing"])
    graph_material(
        pack,
        {
            "Texture2D_a": ("A_BaseColor", BASE),
            "Texture2D_b": ("MaskedColorMask", ORM),
            "Texture2D_c": ("UniformClouds", ORM),
        },
    )

    pack = Pack("orm-map", ["M_Thing"])
    graph_material(
        pack,
        {"Texture2D_a": ("A_BaseColor", BASE), "Texture2D_b": ("ORMMap", ORM)},
    )

    pack = Pack("detail-normal", ["M_Thing"])
    graph_material(
        pack,
        {
            "Texture2D_a": ("A_BaseColor", BASE),
            "Texture2D_b": ("Detail Map Normal", OTHER_BASE),
            "Texture2D_c": ("Macro Normal", NORMAL),
        },
    )

    # `Second Normal` sorts first but isn't the normal map.
    pack = Pack("second-normal", ["M_Thing"])
    graph_material(
        pack,
        {
            "Texture2D_a": ("A_BaseColor", BASE),
            "Texture2D_b": ("Second Normal", OTHER_BASE),
            "Texture2D_c": ("Stone Normal", NORMAL),
        },
    )

    pack = Pack("normals-word", ["M_Thing"])
    graph_material(
        pack,
        {"Texture2D_a": ("A_BaseColor", BASE), "Texture2D_b": ("Stone Normals", NORMAL)},
    )

    pack = Pack("opacity-word", ["M_Thing"])
    graph_material(
        pack,
        {"Texture2D_a": ("A_BaseColor", BASE), "Texture2D_b": ("Opacity", MASK)},
    )

    pack = Pack("mask-only", ["M_Thing"])
    graph_material(
        pack,
        {"Texture2D_a": ("OpacityMap", MASK), "Texture2D_b": ("GlitterTint", ORM)},
        {"_Color": TINT},
    )

    pack = Pack("helper-mask", ["M_Thing"])
    graph_material(
        pack,
        {"Texture2D_a": ("A_BaseColor", BASE), "Texture2D_b": ("GlitterMask", MASK)},
    )

    pack = Pack("base-and-mask", ["M_Thing"])
    graph_material(
        pack,
        {"Texture2D_a": ("A_BaseColor", BASE), "Texture2D_b": ("OpacityMap", MASK)},
    )

    # Raw opacity names, with a base, with none, and ahead of a display-named mask, which is
    # white, so it would cut nothing.
    pack = Pack("raw-mask", ["M_Base", "M_Flat", "M_First"])
    mask = pack.texture("T_Mask", MASK)
    base = pack.texture("T_Base", BASE)
    shader = pack.graph("S_Thing", stream({"Texture2D_x": "A_OpacityMap"}))
    pack.material("M_Base", {"_BaseMap": base, "_OpacityMap": mask})
    pack.material("M_Flat", {"_OpacityMap": mask}, {"_Color": TINT})
    pack.material(
        "M_First",
        {
            "_BaseMap": base,
            "_OpacityMask": mask,
            "Texture2D_x": pack.texture("T_White", (255, 255, 255)),
        },
        shader=shader,
    )

    # A cutout with no mask: the base's own alpha, or nothing to cut with.
    pack = Pack("cutout", ["M_Base", "M_Flat"])
    pack.material("M_Base", {"_BaseMap": pack.texture("T_Base", BASE, 0)}, cutout=True)
    pack.material("M_Flat", colors={"_Color": TINT}, cutout=True)

    # A base whose display name says it holds its alpha, by raw name and by display name, each in
    # an opaque material and a cutout.
    for name, prop, shown in (
        ("base-alpha-raw", "_BaseMap", "Albedo (RGB) Alpha (A)"),
        ("base-alpha-generated", "Texture2D_x", "Albedo Alpha"),
    ):
        pack = Pack(name, ["M_Opaque", "M_Cutout"])
        shader = pack.graph("S_Thing", stream({prop: shown}))
        base = pack.texture("T_Base", BASE, 0)
        pack.material("M_Opaque", {prop: base}, shader=shader)
        pack.material("M_Cutout", {prop: base}, shader=shader, cutout=True)

    # `Albedo Alpha` sorts first, and the real mask must still be found after it.
    pack = Pack("base-alpha-and-mask", ["M_Thing"])
    shader = pack.graph(
        "S_Thing", stream({"Texture2D_a": "Albedo Alpha", "Texture2D_b": "OpacityMap"})
    )
    pack.material(
        "M_Thing",
        {
            "Texture2D_a": pack.texture("T_Base", BASE, 255),
            "Texture2D_b": pack.texture("T_Mask", MASK),
        },
        shader=shader,
    )

    pack = Pack("blank-twin", ["MI_Wall"])
    pack.material("MI_Wall", {"_BaseMap": None})
    pack.material("M_Wall", {"_BaseMap": pack.texture("T_Wall", BASE)})

    pack = Pack("twin-no-mat", ["MI_Wall"])
    pack.material("M_Wall", {"_BaseMap": pack.texture("T_Wall", BASE)})

    pack = Pack("twin-lower-case", ["mi_wall"])
    pack.material("M_Wall", {"_BaseMap": pack.texture("T_Wall", BASE)})

    # `SM_` is no `MI_`, so `M_Wall` is no twin of this slot.
    pack = Pack("not-a-twin", ["SM_Wall"])
    pack.material("SM_Wall", colors={"_BaseColor": TINT})
    pack.material("M_Wall", {"_BaseMap": pack.texture("T_Wall", BASE)})

    pack = Pack("twin-over-colours", ["MI_Wall"])
    pack.material("MI_Wall", colors={"_BaseColor": TINT})
    pack.material("M_Wall", {"_BaseMap": pack.texture("T_Wall", BASE)})

    # A prefab names `MI_Wall.mat`, for a slot of the model's own name.
    pack = Pack("twin-from-prefab", ["Imported"])
    pack.material("MI_Wall", colors={"_BaseColor": TINT})
    pack.material("M_Wall", {"_BaseMap": pack.texture("T_Wall", BASE)})
    pack.meta("Model.obj")
    pack.write(
        "Model.prefab",
        "%YAML 1.1\n"
        "%TAG !u! tag:unity3d.com,2011:\n"
        "--- !u!33 &1\n"
        "MeshFilter:\n"
        "  m_GameObject: {fileID: 7}\n"
        f"  m_Mesh: {{fileID: 4300000, guid: {guid(pack.name, 'Model.obj')}, type: 3}}\n"
        "--- !u!23 &2\n"
        "MeshRenderer:\n"
        "  m_GameObject: {fileID: 7}\n"
        "  m_Materials:\n"
        f"  - {{fileID: 2100000, guid: {guid(pack.name, 'MI_Wall.mat')}, type: 2}}\n",
    )

    # MI_Wall has its own texture, so its twin's isn't wanted.
    pack = Pack("textured-mi", ["MI_Wall"])
    pack.material("MI_Wall", {"_BaseMap": pack.texture("T_Own", OTHER_BASE)})
    pack.material("M_Wall", {"_BaseMap": pack.texture("T_Wall", BASE)})

    # A texture named after the slot, which `named_material` would find: the twin comes first.
    pack = Pack("twin-before-named", ["MI_Wall"])
    pack.material("M_Wall", {"_BaseMap": pack.texture("T_Wall", BASE)})
    pack.texture("MI_Wall_Diffuse", OTHER_BASE)

    # With no twin, `named_material` finds the texture of a slot with no .mat, and of a blank
    # one, but not of a .mat with colours.
    pack = Pack("named-last", ["MI_Gone", "MI_Blank", "MI_Stone"])
    pack.material("MI_Blank", {"_BaseMap": None})
    pack.material("MI_Stone", colors={"_BaseColor": TINT})
    for name in ("MI_Gone", "MI_Blank", "MI_Stone"):
        pack.texture(f"{name}_Diffuse", BASE)

    # MI_Stone has no M_Stone, and MI_Roof's M_Roof has no textures.
    pack = Pack("true-flat-colour", ["MI_Stone", "MI_Roof"])
    pack.material("MI_Stone", colors={"_BaseColor": TINT})
    pack.material("MI_Roof", colors={"_BaseColor": TINT})
    pack.material("M_Roof", {"_BaseMap": None}, {"_BaseColor": GREY})

    pack = Pack("flat-colour", ["M_Thing"])
    shader = pack.graph("S_Thing", stream({"Color_a": "BaseColor Value"}))
    colors = {"Color_a": TINT, "_BaseColor": GREY, "_Color": GREY}
    pack.material("M_Thing", colors=colors, shader=shader)

    # `_Color` is raw, so it beats a display name of the same word.
    pack = Pack("raw-colour-first", ["M_Thing"])
    shader = pack.graph("S_Thing", stream({"Color_a": "Color"}))
    pack.material("M_Thing", colors={"Color_a": GREY, "_Color": TINT}, shader=shader)

    # Both display names read the same, and the one that sorts first wins, not the first property.
    pack = Pack("colour-sort", ["M_Thing"])
    shader = pack.graph("S_Thing", stream({"Color_a": "BaseColor", "Color_b": "Base Color"}))
    pack.material("M_Thing", colors={"Color_a": GREY, "Color_b": TINT}, shader=shader)

    # An HDR colour is stored linear, by its graph's mode, or past 1 in a shader with no graph.
    pack = Pack("hdr-colour", ["M_Graph", "M_Bright"])
    shader = pack.graph("S_Thing", stream({"Color_a": "BaseColor"}, hdr=("Color_a",)))
    pack.material("M_Graph", colors={"Color_a": TINT}, shader=shader)
    pack.material("M_Bright", colors={"_BaseColor": (2, 0.4, 0.6, 1)})

    # Unity makes a submesh per material that faces use, so a renderer's list skips `Empty`.
    pack = Pack("empty-slot")
    pack.write(
        "Model.obj",
        "mtllib Model.mtl\no Model\nusemtl Empty\nv 0 0 0\nv 1 0 0\nv 0 1 0\n"
        "vt 0 0\nvt 1 0\nvt 0 1\nusemtl Imported\nf 1/1 2/2 3/3\n",
    )
    pack.write("Model.mtl", "newmtl Empty\nKd 1 1 1\nnewmtl Imported\nKd 1 1 1\n")
    pack.meta("Model.obj")
    pack.material("M_Real", {"_BaseMap": pack.texture("T_Real", BASE)})
    pack.write(
        "Model.prefab",
        "\n".join(
            [
                "%YAML 1.1",
                "%TAG !u! tag:unity3d.com,2011:",
                "--- !u!33 &1",
                "MeshFilter:",
                "  m_GameObject: {fileID: 7}",
                f"  m_Mesh: {{fileID: 4300000, guid: {guid(pack.name, 'Model.obj')}, type: 3}}",
                "--- !u!23 &2",
                "MeshRenderer:",
                "  m_GameObject: {fileID: 7}",
                "  m_Materials:",
                f"  - {{fileID: 2100000, guid: {guid(pack.name, 'M_Real.mat')}, type: 2}}",
            ]
        )
        + "\n",
    )

    pack = Pack("only-texture", ["M_Thing"])
    pack.material("M_Thing", {"_Grid": pack.texture("T_Grid", BASE)})

    # A base hint and another role's word: `Albedo Mask` sorts first but isn't the base, and a
    # raw `_ColorMask` isn't either.
    pack = Pack("display-not-base", ["M_Thing"])
    graph_material(
        pack,
        {"Texture2D_a": ("Albedo Mask", OTHER_BASE), "Texture2D_b": ("BaseColor", BASE)},
    )

    pack = Pack("raw-not-base", ["M_Thing"])
    shader = pack.graph("S_Thing", stream({"Texture2D_x": "A_BaseColor"}))
    pack.material(
        "M_Thing",
        {
            "_ColorMask": pack.texture("T_Mask", OTHER_BASE),
            "Texture2D_x": pack.texture("T_Base", BASE),
        },
        shader=shader,
    )

    # A raw name with a base hint, in none of the exact names, beats a display name's hint.
    pack = Pack("raw-hint-first", ["M_Thing"])
    shader = pack.graph("S_Thing", stream({"Texture2D_x": "A_BaseColor"}))
    pack.material(
        "M_Thing",
        {
            "Texture2D_x": pack.texture("T_Layer", OTHER_BASE),
            "_LayerBaseColor": pack.texture("T_Base", BASE),
        },
        shader=shader,
    )

    # Two textures nothing claims: there's no only texture, so the colour stays.
    pack = Pack("two-textures", ["M_Thing"])
    pack.material(
        "M_Thing",
        {"_Grid": pack.texture("T_Grid", BASE), "_Other": pack.texture("T_Other", OTHER_BASE)},
        {"_Color": TINT},
    )

    # A generated name with a display name is no generated name: it's the material's only texture.
    pack = Pack("lone-neutral", ["M_Thing"])
    graph_material(pack, {"Texture2D_x": ("Grid", BASE)}, {"_Color": TINT})

    pack = Pack("generated-slot", ["M_Thing"])
    pack.material(
        "M_Thing",
        {"_SampleTexture2D_abc123_Texture_1": pack.texture("T_Sample", BASE)},
        {"_Color": TINT},
    )

    pack = Pack("lone-mask", ["M_Thing"])
    graph_material(pack, {"Texture2D_x": ("OpacityMap", MASK)}, {"_Color": TINT})

    pack = Pack("lone-normal", ["M_Thing"])
    graph_material(pack, {"Texture2D_x": ("NormalMap", NORMAL)}, {"_Color": TINT})

    # A NOT_BASE word in one name or the other, and no role that would refuse it anyway.
    pack = Pack("lone-not-base", ["M_Raw", "M_Shown"])
    pack.material("M_Raw", {"_EmissionMap": pack.texture("T_Glow", BASE)}, {"_Color": TINT})
    shader = pack.graph("S_Thing", stream({"Texture2D_x": "Roughness"}))
    pack.material(
        "M_Shown", {"Texture2D_x": pack.texture("T_Rough", BASE)}, {"_Color": TINT}, shader
    )

    # Takes a role, though no NOT_BASE word says so.
    pack = Pack("lone-alpha", ["M_Thing"])
    graph_material(pack, {"Texture2D_x": ("Alpha", MASK)}, {"_Color": TINT})

    pack = Pack("lone-arm", ["M_Thing"])
    pack.material("M_Thing", {"_ARM": pack.texture("T_Arm", ORM)}, {"_Color": TINT})

    # A byte order mark doesn't make a graph unreadable.
    pack = Pack("bom-graph", ["M_Thing"])
    shader = pack.graph("S_Thing", "\ufeff" + stream({"Texture2D_x": "A_BaseColor"}))
    pack.material("M_Thing", {"Texture2D_x": pack.texture("T_Base", BASE)}, shader=shader)

    # `_BaseMap` and `_Normal` are found by their raw names, whatever the graph shows.
    pack = Pack("raw-first", ["M_Thing"])
    shader = pack.graph(
        "S_Thing",
        stream(
            {
                "Texture2D_a": "A_BaseColor",
                "_BaseMap": "Detail",
                "_Normal": "Second",
                "Texture2D_b": "A_Normal",
            }
        ),
    )
    pack.material(
        "M_Thing",
        {
            "Texture2D_a": pack.texture("T_Layer", OTHER_BASE),
            "Texture2D_b": pack.texture("T_LayerNormal", OTHER_BASE),
            "_BaseMap": pack.texture("T_Base", BASE),
            "_Normal": pack.texture("T_Normal", NORMAL),
        },
        shader=shader,
    )

    # Not an object, a name that isn't a string, and a reference that isn't, among properties
    # that read well.
    good = stream({"Texture2D_x": "A_BaseColor", "Texture2D_y": "Other"})
    odd_graph = "[1]\n\n" + good.replace('"m_Name": "A_BaseColor"', '"m_Name": 7').replace(
        '"m_DefaultReferenceName": "Texture2D_y"', '"m_DefaultReferenceName": [1]'
    )
    # A keyword has a name and a reference too, but is no property.
    keyword = {
        "m_Type": "UnityEditor.ShaderGraph.ShaderKeyword",
        "m_Name": "A_BaseColor",
        "m_DefaultReferenceName": "Texture2D_x",
    }

    # None of these graphs gives a name, even the one that reads well up to its last object, so
    # each lone texture stays unresolved.
    cut_short = stream({"Texture2D_x": "A_BaseColor", "Texture2D_y": "Other"})[:-40]
    for name, text in (
        ("older-graph", older({"Texture2D_x": "A_BaseColor"})),
        ("unreadable-graph", cut_short),
        ("odd-graph", odd_graph),
        ("keyword-graph", json.dumps(keyword)),
    ):
        pack = Pack(name, ["M_Thing"])
        shader = pack.graph("S_Thing", text)
        pack.material(
            "M_Thing", {"Texture2D_x": pack.texture("T_Base", BASE)}, {"_Color": TINT}, shader
        )

    # A shader that clips sets no keyword in the material. These four clip on their base's
    # alpha: a graph's target, an older graph's threshold, one with `FAE_TreeBranch`'s shape (a
    # linked threshold of 0, and its alpha through a sub graph) and a `.shader`'s tags. The other
    # six don't: a `.shader` with no cutout tag, a graph leaving the clip to the material, one
    # whose target doesn't clip, one whose Alpha reads the base's red, one that clips on another
    # texture's alpha, and an older graph whose threshold is 0.
    pack = Pack(
        "shader-clip",
        [
            "M_Graph",
            "M_Older",
            "M_Linked",
            "M_Shader",
            "M_Opaque",
            "M_Override",
            "M_NoClip",
            "M_Red",
            "M_OtherAlpha",
            "M_NoThreshold",
        ],
    )
    base = pack.texture("T_Base", BASE, 0)
    other = pack.texture("T_Other", MASK, 0)
    for name, prop, shader, file_id in (
        ("M_Graph", "_BaseMap", pack.graph("S_Graph", clip_stream("_BaseMap")), GRAPH_FILE_ID),
        ("M_Older", "_MainTex", pack.graph("S_Older", older_clip("_MainTex", 0.5)), GRAPH_FILE_ID),
        (
            "M_Linked",
            "_MainTex",
            pack.graph("S_Linked", older_clip("_MainTex", 0, linked=True)),
            GRAPH_FILE_ID,
        ),
        (
            "M_Shader",
            "_MainTex",
            pack.shader("S_Cutout", shader_source("TransparentCutout")),
            SHADER_FILE_ID,
        ),
        ("M_Opaque", "_MainTex", pack.shader("S_Opaque", shader_source("Opaque")), SHADER_FILE_ID),
        (
            "M_Override",
            "_BaseMap",
            pack.graph("S_Override", clip_stream("_BaseMap", override=True)),
            GRAPH_FILE_ID,
        ),
        (
            "M_NoClip",
            "_BaseMap",
            pack.graph("S_NoClip", clip_stream("_BaseMap", clip=False)),
            GRAPH_FILE_ID,
        ),
        (
            "M_Red",
            "_BaseMap",
            pack.graph("S_Red", clip_stream("_BaseMap", alpha_from=4)),
            GRAPH_FILE_ID,
        ),
        (
            "M_NoThreshold",
            "_MainTex",
            pack.graph("S_NoThreshold", older_clip("_MainTex", 0)),
            GRAPH_FILE_ID,
        ),
    ):
        pack.material(name, {prop: base}, shader=shader, file_id=file_id)
    # The base is `_BaseMap`, and the graph clips on `_Detail`'s alpha.
    shader = pack.graph("S_OtherAlpha", clip_stream("_Detail", shown="Detail"))
    pack.material("M_OtherAlpha", {"_BaseMap": base, "_Detail": other}, shader=shader)

    Pack("lod-only").objects(["Model_LOD1"])

    # C's lowest level isn't 1, and listed last it doesn't come first.
    Pack("lod-mixed").objects(["A_LOD0", "A_LOD1", "B_LOD1", "C_LOD3", "C_LOD2"])

    # `Wall` and `Wall_LOD0` are both level 0, and the case of a suffix doesn't count.
    Pack("lod-no-suffix").objects(["Wall", "Wall_LOD1", "Roof_LOD0", "Roof", "Roof_lod2"])

    # Two objects of one name, which Blender's importer keeps apart by these suffixes, and one
    # above their level, which goes only if the suffixes are read past.
    Pack("lod-several").objects(["X_LOD1.001", "X_LOD1.002", "X_LOD2.001"])

    # A collision mesh goes even here, where every level stays.
    Pack("lod-file").objects(["X_LOD1", "X_LOD2", "X_Collider_LOD1"], "X_LOD1")

    # `_LOD0` is level 0, so this file is no LOD and its other levels go.
    Pack("lod0-file").objects(["X_LOD0", "X_LOD1"], "X_LOD0")

    Pack("no-mesh").objects(["UCX_Model", "Model_Collider"])

    # `_LOD<n>` hides no collision mesh, not even at the lowest level of its name.
    Pack("no-mesh-lod").objects(["Model_Collider_LOD0", "Model_Collider_LOD1"])

    Pack("collider-file").objects(["rock_collider"], "rock_collider")

    Pack("collider-lod-file").objects(["Rock_Collider_LOD1"], "Rock_Collider_LOD1")

    pack = Pack("exact-name")
    pack.objects(["Plane.099", "Plane.100"])
    mapped_planes(pack, "Model.obj")

    mapped_planes(Pack("mesh-name"), "Model.fbx")

    mapped_planes(Pack("exact-first"), "Model.fbx", plain=True)


main()
