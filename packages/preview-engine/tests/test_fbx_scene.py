"""FBX/3DS previews built from assimp's node tree (plan textured-fbx-previews, C2, C4 and C7).

The scenes are fake assimp-like objects handed to `model_meta` in place of
`assimp_py.import_file`, so no binary FBX is committed. Each test reads the exported GLB back.
"""

from __future__ import annotations

import io
import json
import math
import struct
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np
import pytest
from PIL import Image

import textures
import worker

IDENTITY = (1, 0, 0, 0), (0, 1, 0, 0), (0, 0, 1, 0), (0, 0, 0, 1)


def translation(x: float, y: float, z: float):
    return (1, 0, 0, x), (0, 1, 0, y), (0, 0, 1, z), (0, 0, 0, 1)


# Z-up to Y-up, as a Unity FBX root carries it: -90 degrees about X.
Z_UP_ROOT = (1, 0, 0, 0), (0, 0, 1, 0), (0, -1, 0, 0), (0, 0, 0, 1)


@dataclass
class FakeNode:
    name: str
    mesh_indices: tuple[int, ...] = ()
    children: list[FakeNode] = field(default_factory=list)
    transformation: tuple = IDENTITY


@dataclass
class FakeMesh:
    """Flat buffers, as assimp_py exposes them."""

    vertices: list[float]
    indices: list[int]
    num_faces: int
    normals: list[float] | None = None
    texcoords: list[list[float]] | None = None
    num_uv_components: list[int] = field(default_factory=list)
    material_index: int = 0


@dataclass
class FakeScene:
    root_node: FakeNode
    meshes: list[FakeMesh]
    materials: list[dict] = field(default_factory=lambda: [{"NAME": "DefaultMaterial"}])


def triangle(**kwargs) -> FakeMesh:
    """One triangle in the XY plane at the origin, 1 unit on a side."""
    return FakeMesh(vertices=[0, 0, 0, 1, 0, 0, 0, 1, 0], indices=[0, 1, 2], num_faces=1, **kwargs)


def preview(tmp_path, monkeypatch, scene: FakeScene, ext: str = ".fbx") -> dict:
    """Run `model_meta` on a stand-in file that imports as `scene`."""
    import assimp_py

    source = tmp_path / f"model{ext}"
    source.write_bytes(b"fake")
    monkeypatch.setattr(assimp_py, "import_file", lambda path, flags: scene)
    return worker.model_meta(str(source), str(tmp_path / "thumbs"), "42")


def read_glb(path: str) -> tuple[dict, bytes]:
    """The JSON and BIN chunks of a GLB file."""
    data = open(path, "rb").read()
    magic, _version, _length = struct.unpack_from("<4sII", data, 0)
    assert magic == b"glTF"
    offset, chunks = 12, {}
    while offset < len(data):
        length, kind = struct.unpack_from("<I4s", data, offset)
        chunks[kind] = data[offset + 8 : offset + 8 + length]
        offset += 8 + length
    return json.loads(chunks[b"JSON"]), chunks[b"BIN\x00"]


def accessor(gltf: dict, binary: bytes, index: int) -> np.ndarray:
    """A float accessor's values, one row per element."""
    acc = gltf["accessors"][index]
    view = gltf["bufferViews"][acc["bufferView"]]
    width = {"VEC2": 2, "VEC3": 3, "VEC4": 4}[acc["type"]]
    start = view.get("byteOffset", 0) + acc.get("byteOffset", 0)
    values = np.frombuffer(binary, dtype=np.float32, count=acc["count"] * width, offset=start)
    return values.reshape(-1, width)


def node_matrices(gltf: dict) -> dict[str, np.ndarray]:
    """Each mesh node's world matrix by node name, for a scene of depth one or more."""
    out = {}

    def walk(index: int, parent: np.ndarray) -> None:
        node = gltf["nodes"][index]
        local = np.eye(4)
        if "matrix" in node:
            local = np.array(node["matrix"]).reshape(4, 4).T  # glTF is column-major
        world = parent @ local
        if "mesh" in node:
            out[node["name"]] = world
        for child in node.get("children", []):
            walk(child, world)

    for root in gltf["scenes"][gltf.get("scene", 0)]["nodes"]:
        walk(root, np.eye(4))
    return out


def drawn_names(gltf: dict) -> set[str]:
    """The assimp node names drawn, without the `_<n>` instance number."""
    return {name.rsplit("_", 1)[0] for name in node_matrices(gltf)}


def test_node_transforms_place_parts_and_stand_z_up_models_upright(tmp_path, monkeypatch):
    # A Z-up root holding the same mesh twice, at x = 5 and x = -5.
    scene = FakeScene(
        root_node=FakeNode(
            "Root",
            transformation=Z_UP_ROOT,
            children=[
                FakeNode("Left", (0,), transformation=translation(5, 0, 0)),
                FakeNode("Right", (0,), transformation=translation(-5, 0, 0)),
            ],
        ),
        # A triangle standing up along Z, as authored in a Z-up tool.
        meshes=[FakeMesh(vertices=[0, 0, 0, 1, 0, 0, 0, 0, 1], indices=[0, 1, 2], num_faces=1)],
    )
    res = preview(tmp_path, monkeypatch, scene)

    assert res["ok"], res
    assert res["metadata"]["object_count"] == 2
    assert res["metadata"]["poly_count"] == 2
    gltf, binary = read_glb(res["preview_model"])
    assert len(gltf["meshes"]) == 1, "an instanced mesh is stored once"
    matrices = node_matrices(gltf)
    assert set(matrices) == {"Left_0", "Right_1"}

    positions = accessor(gltf, binary, gltf["meshes"][0]["primitives"][0]["attributes"]["POSITION"])
    homogeneous = np.c_[positions, np.ones(len(positions))]
    left = (matrices["Left_0"] @ homogeneous.T).T[:, :3]
    right = (matrices["Right_1"] @ homogeneous.T).T[:, :3]
    # The root's rotation turns authored +Z into +Y, so the triangle stands upright.
    np.testing.assert_allclose(left, [[5, 0, 0], [6, 0, 0], [5, 1, 0]], atol=1e-6)
    np.testing.assert_allclose(right, [[-5, 0, 0], [-4, 0, 0], [-5, 1, 0]], atol=1e-6)


def test_nested_transforms_multiply_into_the_parent(tmp_path, monkeypatch):
    scene = FakeScene(
        root_node=FakeNode(
            "Root",
            transformation=translation(0, 10, 0),
            children=[
                FakeNode(
                    "Body",
                    transformation=translation(1, 0, 0),
                    children=[FakeNode("Lid", (0,), transformation=translation(0, 0, 2))],
                )
            ],
        ),
        meshes=[triangle()],
    )
    res = preview(tmp_path, monkeypatch, scene)

    gltf, _ = read_glb(res["preview_model"])
    np.testing.assert_allclose(node_matrices(gltf)["Lid_0"][:3, 3], [1, 10, 2])


SKIPPED = [
    "Tree_LOD1",
    "Tree_LOD01",
    "Tree_LOD_2",
    "MapleSpringTree04LOD3",
    "TreeLOD2",
    "tree_lod1",
    "LOD12",
    "UCX_Crate",
    "ubx_crate_01",
    "USP_Ball",
    "Ucp_Pipe",
    "Rock_ConvexHulls",
    "CONVEXHULL_3",
    "Tree_LOD1_$AssimpFbx$_PreRotation",
    "UCX_Crate_$AssimpFbx$_Translation",
]
DRAWN = [
    "Tree_LOD0",
    "Tree_LOD00",
    "SH_Shield_E_LOD",
    "Ballod2",
    "Tree",
    "Tree_LOD0_$AssimpFbx$_PreRotation",
    "LODGE",
    "Crate_UCX",
]


@pytest.mark.parametrize("name", SKIPPED)
def test_lod_and_collision_nodes_are_skipped(name):
    assert worker.is_skipped_node(name)


@pytest.mark.parametrize("name", DRAWN)
def test_lod0_and_ordinary_nodes_are_drawn(name):
    assert not worker.is_skipped_node(name)


def test_skipped_nodes_take_their_subtree_along(tmp_path, monkeypatch):
    # Each node draws its own copy of the triangle; the named children sit under the root.
    names = [*SKIPPED, *DRAWN]
    children = [FakeNode(name, (i,)) for i, name in enumerate(names)]
    # A helper-suffixed LOD1 wrapper hides its child, which is drawn anywhere else.
    children.append(
        FakeNode(
            "Bush_LOD1_$AssimpFbx$_PreRotation",
            children=[FakeNode("Bush_Leaves", (len(names),))],
        )
    )
    children.append(
        FakeNode(
            "Bush_LOD0_$AssimpFbx$_PreRotation",
            children=[FakeNode("Bush_Trunk", (len(names) + 1,))],
        )
    )
    scene = FakeScene(
        root_node=FakeNode("Root", children=children),
        meshes=[triangle() for _ in range(len(names) + 2)],
    )
    res = preview(tmp_path, monkeypatch, scene)

    gltf, _ = read_glb(res["preview_model"])
    assert drawn_names(gltf) == {*DRAWN, "Bush_Trunk"}
    assert res["metadata"]["object_count"] == len(DRAWN) + 1
    assert res["metadata"]["poly_count"] == len(DRAWN) + 1


def test_everything_is_drawn_when_skipping_leaves_nothing(tmp_path, monkeypatch):
    scene = FakeScene(
        root_node=FakeNode(
            "Root",
            children=[FakeNode("Tree_LOD1", (0,)), FakeNode("UCX_Tree", (1,))],
        ),
        meshes=[triangle(), triangle()],
    )
    res = preview(tmp_path, monkeypatch, scene)

    assert res["ok"], res
    gltf, _ = read_glb(res["preview_model"])
    assert drawn_names(gltf) == {"Tree_LOD1", "UCX_Tree"}
    assert res["metadata"]["object_count"] == 2


def test_normals_and_uv_channel_0_are_kept(tmp_path, monkeypatch):
    normals = [0, 0, -1, 0, 0.6, 0.8, 1, 0, 0]
    scene = FakeScene(
        root_node=FakeNode("Root", (0, 1)),
        meshes=[
            # UV channel 0 with three components, the third dropped; channel 1 ignored.
            triangle(
                normals=normals,
                texcoords=[[0.1, 0.2, 9, 0.3, 0.4, 9, 0.5, 0.6, 9], [7, 7, 7, 7, 7, 7]],
                num_uv_components=[3, 2],
            ),
            # No UVs at all.
            triangle(normals=[0, 0, 1] * 3),
        ],
    )
    res = preview(tmp_path, monkeypatch, scene)

    gltf, binary = read_glb(res["preview_model"])
    with_uv = gltf["meshes"][0]["primitives"][0]["attributes"]
    np.testing.assert_allclose(accessor(gltf, binary, with_uv["NORMAL"]).ravel(), normals)
    # glTF puts the UV origin at the top left, so trimesh writes v as 1 - v.
    np.testing.assert_allclose(
        accessor(gltf, binary, with_uv["TEXCOORD_0"]),
        [[0.1, 0.8], [0.3, 0.6], [0.5, 0.4]],
        atol=1e-6,
    )
    without_uv = gltf["meshes"][1]["primitives"][0]["attributes"]
    assert "NORMAL" in without_uv
    assert "TEXCOORD_0" not in without_uv


def test_materials_are_pbr_in_the_diffuse_color_or_gray(tmp_path, monkeypatch):
    scene = FakeScene(
        root_node=FakeNode("Root", (0, 1)),
        meshes=[triangle(material_index=0), triangle(material_index=1)],
        materials=[
            {"NAME": "Red", "COLOR_DIFFUSE": [1.0, 0.0, 0.0], "OPACITY": 0.5, "TEXTURES": {}},
            {"NAME": "Plain", "TEXTURES": {}},
        ],
    )
    res = preview(tmp_path, monkeypatch, scene)

    gltf, _ = read_glb(res["preview_model"])
    by_name = {m["name"]: m for m in gltf["materials"]}
    assert set(by_name) == {"Red", "Plain"}
    for material in by_name.values():
        pbr = material["pbrMetallicRoughness"]
        assert pbr["metallicFactor"] == 0
        assert math.isclose(pbr["roughnessFactor"], 0.9, abs_tol=1e-6)
        assert "baseColorTexture" not in pbr
        assert material.get("alphaMode", "OPAQUE") == "OPAQUE"
    # trimesh stores colors as 8-bit, so factors come back within 1/255.
    np.testing.assert_allclose(
        by_name["Red"]["pbrMetallicRoughness"]["baseColorFactor"], [1, 0, 0, 0.5], atol=1 / 255
    )
    np.testing.assert_allclose(
        by_name["Plain"]["pbrMetallicRoughness"]["baseColorFactor"],
        [0.8, 0.8, 0.8, 1],
        atol=1 / 255,
    )


def test_import_uses_c2_flags(tmp_path, monkeypatch):
    # SortByPType keeps points and lines out of triangle meshes, which `_fbx_geometry`
    # relies on to tell a triangle mesh by its index count.
    import assimp_py

    calls = []
    scene = FakeScene(root_node=FakeNode("Root", (0,)), meshes=[triangle()])
    monkeypatch.setattr(assimp_py, "import_file", lambda path, flags: calls.append(flags) or scene)
    source = tmp_path / "model.fbx"
    source.write_bytes(b"fake")
    worker.model_meta(str(source), str(tmp_path / "thumbs"), "42")

    assert calls == [
        assimp_py.Process_Triangulate
        | assimp_py.Process_GenNormals
        | assimp_py.Process_JoinIdenticalVertices
        | assimp_py.Process_SortByPType
    ]


def test_3ds_goes_through_the_same_scene(tmp_path, monkeypatch):
    scene = FakeScene(root_node=FakeNode("Root", (0,)), meshes=[triangle()])
    res = preview(tmp_path, monkeypatch, scene, ext=".3ds")

    assert res["ok"], res
    assert res["preview_model"].endswith("42_preview.glb")


def test_an_import_error_fails_with_preview_error(tmp_path, monkeypatch):
    import assimp_py

    def fail(path, flags):
        raise RuntimeError("")  # assimp_py's message for an animation-only FBX

    source = tmp_path / "anim.fbx"
    source.write_bytes(b"fake")
    monkeypatch.setattr(assimp_py, "import_file", fail)
    res = worker.model_meta(str(source), str(tmp_path / "thumbs"), "42")

    assert res["ok"] is False
    assert res["preview_error"].startswith("FBX/3DS convert failed")
    assert "preview_model" not in res


def test_a_scene_with_nothing_to_draw_fails_with_preview_error(tmp_path, monkeypatch):
    # Points and lines are skipped, and a mesh no node uses is never drawn.
    lines = FakeMesh(vertices=[0, 0, 0, 1, 0, 0, 0, 1, 0], indices=[0, 1, 1, 2], num_faces=2)
    points = FakeMesh(vertices=[0, 0, 0, 1, 0, 0, 0, 1, 0], indices=[0, 1, 2], num_faces=3)
    scene = FakeScene(
        root_node=FakeNode("Root", (0, 1), children=[FakeNode("Tree_LOD1")]),
        meshes=[lines, points, triangle()],
    )
    res = preview(tmp_path, monkeypatch, scene)

    assert res["ok"] is False
    assert res["preview_error"] == "No valid meshes in FBX/3DS file"
    assert not (tmp_path / "thumbs" / "42_preview.glb").exists()


# ── Base color textures ───────────────────────────────────────────────────

UV = {"texcoords": [[0, 0, 1, 0, 0, 1]], "num_uv_components": [2]}


@pytest.fixture(autouse=True)
def fresh_index_cache():
    textures.clear_index_cache()


def textured_preview(tmp_path, monkeypatch, scene: FakeScene, images: dict[str, Image.Image]):
    """
    Run `model_meta` on `lib/Pack/Meshes/model.fbx`, with `images` saved under `lib/Pack`
    and `lib` and `lib/Pack` sent as the library and pack roots.
    """
    import assimp_py

    pack = tmp_path / "lib" / "Pack"
    for rel, img in images.items():
        (pack / rel).parent.mkdir(parents=True, exist_ok=True)
        img.save(pack / rel)
    source = pack / "Meshes" / "model.fbx"
    source.parent.mkdir(parents=True, exist_ok=True)
    source.write_bytes(b"fake")
    monkeypatch.setattr(assimp_py, "import_file", lambda path, flags: scene)
    return worker.model_meta(
        str(source), str(tmp_path / "thumbs"), "42", str(tmp_path / "lib"), str(pack)
    )


def one_material(material: dict, **mesh) -> FakeScene:
    return FakeScene(
        root_node=FakeNode("Root", (0,)), meshes=[triangle(**mesh)], materials=[material]
    )


def glb_images(gltf: dict, binary: bytes) -> list[tuple[str, Image.Image]]:
    """Each image in the GLB as `(mimeType, decoded image)`."""
    out = []
    for image in gltf.get("images", []):
        view = gltf["bufferViews"][image["bufferView"]]
        start = view.get("byteOffset", 0)
        data = binary[start : start + view["byteLength"]]
        out.append((image["mimeType"], Image.open(io.BytesIO(data))))
    return out


def rgba(size, alpha) -> Image.Image:
    img = Image.new("RGBA", size, (90, 140, 60, 255))
    img.putalpha(alpha)
    return img


def test_a_resolved_texture_is_the_base_color_on_white(tmp_path, monkeypatch):
    scene = one_material({"NAME": "Rock", "TEXTURES": {1: ["..\\Textures\\rock.png"]}}, **UV)
    res = textured_preview(
        tmp_path, monkeypatch, scene, {"Textures/rock.png": Image.new("RGB", (2048, 1024))}
    )

    assert res["ok"], res
    gltf, binary = read_glb(res["preview_model"])
    pbr = gltf["materials"][0]["pbrMetallicRoughness"]
    assert "baseColorTexture" in pbr
    np.testing.assert_allclose(pbr.get("baseColorFactor", [1, 1, 1, 1]), [1, 1, 1, 1])
    assert gltf["materials"][0].get("alphaMode", "OPAQUE") == "OPAQUE"
    [(mime, img)] = glb_images(gltf, binary)
    assert mime == "image/jpeg"
    assert img.size == (1024, 512)
    report = res["preview_textures"]
    assert report["textured"] == 1
    assert report["via"] == {"relative": 1}
    assert report["alpha"] == {"cutout": 0, "ignored": 0, "none": 1}


def test_a_cutout_texture_is_a_double_sided_png_mask(tmp_path, monkeypatch):
    alpha = Image.new("L", (2048, 2048), 0)
    alpha.paste(255, (512, 0, 1536, 2048))
    scene = one_material({"NAME": "Leaves", "TEXTURES": {12: ["C:\\leaves.tga"]}}, **UV)
    res = textured_preview(
        tmp_path, monkeypatch, scene, {"Textures/leaves.png": rgba((2048, 2048), alpha)}
    )

    gltf, binary = read_glb(res["preview_model"])
    material = gltf["materials"][0]
    assert material["alphaMode"] == "MASK"
    assert material["alphaCutoff"] == 0.5
    assert material["doubleSided"] is True
    [(mime, img)] = glb_images(gltf, binary)
    assert mime == "image/png"
    assert img.mode == "RGBA"
    assert max(img.size) <= 1024
    assert res["preview_textures"]["via"] == {"name": 1}
    assert res["preview_textures"]["alpha"] == {"cutout": 1, "ignored": 0, "none": 0}


def test_all_zero_alpha_is_opaque_and_jpeg(tmp_path, monkeypatch):
    scene = one_material({"NAME": "Crate", "TEXTURES": {1: ["crate.tga"]}}, **UV)
    res = textured_preview(tmp_path, monkeypatch, scene, {"Meshes/crate.tga": rgba((64, 64), 0)})

    gltf, binary = read_glb(res["preview_model"])
    assert gltf["materials"][0].get("alphaMode", "OPAQUE") == "OPAQUE"
    [(mime, _)] = glb_images(gltf, binary)
    assert mime == "image/jpeg"
    assert res["preview_textures"]["alpha"] == {"cutout": 0, "ignored": 1, "none": 0}


def test_a_missing_texture_leaves_a_valid_colored_preview(tmp_path, monkeypatch):
    scene = one_material(
        {"NAME": "Red", "COLOR_DIFFUSE": [1, 0, 0], "TEXTURES": {1: ["..\\T_Fill_B.png"]}},
        **UV,
    )
    res = textured_preview(tmp_path, monkeypatch, scene, {})

    assert res["ok"], res
    gltf, _ = read_glb(res["preview_model"])
    assert "images" not in gltf
    pbr = gltf["materials"][0]["pbrMetallicRoughness"]
    assert "baseColorTexture" not in pbr
    np.testing.assert_allclose(pbr["baseColorFactor"], [1, 0, 0, 1], atol=1 / 255)
    assert res["preview_textures"]["missing"] == ["T_Fill_B"]
    assert res["preview_textures"]["textured"] == 0


def test_an_unreadable_texture_leaves_a_valid_colored_preview(tmp_path, monkeypatch):
    scene = one_material({"NAME": "Rock", "TEXTURES": {1: ["rock.png"]}}, **UV)
    bad = tmp_path / "lib" / "Pack" / "Meshes" / "rock.png"
    bad.parent.mkdir(parents=True)
    bad.write_bytes(b"not a png")
    res = textured_preview(tmp_path, monkeypatch, scene, {})

    assert res["ok"], res
    assert "images" not in read_glb(res["preview_model"])[0]
    assert res["preview_textures"]["unreadable"] == ["rock.png"]


def test_a_mesh_without_uvs_keeps_the_color(tmp_path, monkeypatch):
    scene = FakeScene(
        root_node=FakeNode("Root", (0, 1)),
        meshes=[triangle(**UV), triangle()],
        materials=[{"NAME": "Rock", "COLOR_DIFFUSE": [0, 0, 1], "TEXTURES": {1: ["rock.png"]}}],
    )
    res = textured_preview(
        tmp_path, monkeypatch, scene, {"Meshes/rock.png": Image.new("RGB", (8, 8))}
    )

    gltf, _ = read_glb(res["preview_model"])
    by_mesh = [gltf["materials"][m["primitives"][0]["material"]] for m in gltf["meshes"]]
    assert "baseColorTexture" in by_mesh[0]["pbrMetallicRoughness"]
    assert "baseColorTexture" not in by_mesh[1]["pbrMetallicRoughness"]
    report = res["preview_textures"]
    assert (report["materials"], report["textured"], report["no_uv"]) == (1, 1, 1)


def test_a_shared_atlas_is_decoded_once(tmp_path, monkeypatch):
    atlas = {"TEXTURES": {1: ["atlas.png"]}}
    scene = FakeScene(
        root_node=FakeNode("Root", (0, 1)),
        meshes=[triangle(material_index=0, **UV), triangle(material_index=1, **UV)],
        materials=[{"NAME": "A", **atlas}, {"NAME": "B", **atlas}],
    )
    calls = []
    real = textures._load
    monkeypatch.setattr(textures, "_load", lambda p: calls.append(p) or real(p))
    res = textured_preview(
        tmp_path, monkeypatch, scene, {"Textures/atlas.png": Image.new("RGB", (32, 32))}
    )

    assert len(calls) == 1
    assert res["preview_textures"]["textured"] == 2


def test_without_a_library_root_no_texture_is_read(tmp_path, monkeypatch):
    # The texture sits next to the model, but no root bounds the lookup.
    Image.new("RGB", (8, 8)).save(tmp_path / "rock.png")
    monkeypatch.setattr(textures, "load_texture", pytest.fail)
    scene = one_material(
        {"NAME": "Rock", "COLOR_DIFFUSE": [0, 1, 0], "TEXTURES": {1: ["rock.png"]}}, **UV
    )
    res = preview(tmp_path, monkeypatch, scene)

    assert res["ok"], res
    gltf, _ = read_glb(res["preview_model"])
    assert "images" not in gltf
    assert res["preview_textures"]["no_root"] is True
    assert res["preview_textures"]["textured"] == 0


def test_the_report_counts_drawn_and_skipped_nodes(tmp_path, monkeypatch):
    scene = FakeScene(
        root_node=FakeNode(
            "Root",
            children=[
                FakeNode("Tree_LOD0", (0,)),
                FakeNode("Tree_LOD1", (0,), children=[FakeNode("Leaves", (0,))]),
                FakeNode("UCX_Tree", (0,)),
                FakeNode("Twin", (0,)),
            ],
        ),
        meshes=[triangle()],
    )
    res = preview(tmp_path, monkeypatch, scene)

    report = res["preview_textures"]
    assert (report["drawn_nodes"], report["skipped_nodes"]) == (2, 2)
    assert set(report) == {
        "materials",
        "textured",
        "via",
        "missing",
        "unreadable",
        "too_large",
        "no_uv",
        "alpha",
        "drawn_nodes",
        "skipped_nodes",
        "no_root",
    }


def test_the_request_params_reach_the_lookup(tmp_path, monkeypatch):
    import assimp_py

    lib = tmp_path / "lib"
    source = lib / "Pack" / "Meshes" / "model.fbx"
    source.parent.mkdir(parents=True)
    source.write_bytes(b"fake")
    Image.new("RGB", (8, 8)).save(lib / "Pack" / "rock.png")
    scene = one_material({"NAME": "Rock", "TEXTURES": {1: ["rock.png"]}}, **UV)
    monkeypatch.setattr(assimp_py, "import_file", lambda path, flags: scene)
    params = {
        "source": str(source),
        "output_dir": str(tmp_path / "thumbs"),
        "name": "42",
        "library_root": str(lib),
        "pack_root": str(lib / "Pack"),
    }
    res = worker.DISPATCH["model_meta"](params)

    assert res["preview_textures"]["via"] == {"name": 1}
    assert Path(res["preview_model"]).name == "42_preview.glb"
