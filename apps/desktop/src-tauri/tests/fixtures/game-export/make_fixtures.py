"""Writes the game export's fixture library (plan game-export C16) into `library/` beside this
script, replacing what is there. Run it with Blender, which makes the FBX, JPG and EXR files:

    blender -b --factory-startup -P make_fixtures.py

Its output is committed, so no test needs Blender to read it. PNG, JSON and text files are
written byte for byte by this script, so a rerun gives the same bytes. The library grows with the
plan's build order: each fixture lands in the first step whose tests use it.

Step 1 (domains, default names, packs and licences, and the build-order guards):

- `Dungeon Props/`: a `datapackage.json` with a `LicenseRef-` licence, pass-through PNG
  textures, and one file for each guard and refusal: an ORM map, a DirectX normal map, a JPG, an
  EXR, a format group of an FBX and an OBJ, a lone OBJ and a Markdown note.
- `Kenney Crates/`: a CC0 Kenney license file, a `banner.png` whose default name and role
  collide with `Dungeon Props/Textures/T_Banner_BaseColor.png`, and a `___.png` whose stem gives
  no PascalCase name.
- `Unity Moss/`: an MIT license file and a texture with a Unity `.meta` beside it (origin unity).
- `Converted/conversion.json` with a `from: "uasset"` step, above the pack `Converted/Ruins/`
  (origin unreal, so its normal map is DirectX). From step 3 its textures carry that step.
- `Foo A/`, `Foo B/` and `Foo Two/`: packs named `foo`, `foo` and `foo-2`.
- `---/`: a pack whose folder slugifies to nothing and whose license names none (`Unknown`).
- `Loose/stray.png`: an image in no pack.

Step 3 (conversion chains):

- `Scans/`: a pack with its own `conversion.json`, covering `T_Boulder_D.png`, and a deeper one in
  `Scans/Cliffs/` with two steps, which replaces it for `T_Cliff_D.png`.

Step 4 (`fbx_facts`, and the Check telling models from animations), in `Dungeon Props/Models/`:

- `SM_Totem.fbx`: centimetres and Z up, off centre, with an asymmetric nose on its front.
- `SM_Pillar.fbx`: Unreal style, `_LOD0` and `_LOD1` with two slots and a `UCX_` hull with its
  own material. `SM_Gate.fbx`: the same, with a hull that has no material.
- `SM_Boulder.fbx`: no material at all. `SK_Guard.fbx`: skinned, with the take `Wave`.
- `SM_Hull.fbx`: only a `UCX_` hull and no take, which is refused.
- `SM_Stool.fbx`: one material named `Material`. `SM_Bench.fbx`: two materials named `Wood`,
  which is refused. `SM_Lamp.fbx`: a real `DefaultMaterial` beside a mesh with no material.
  `SM_Shelf.fbx`: a slot named `OpenDAMDefault` beside a mesh with no material.

After a rerun, refresh `fbx_facts.json` beside the library, as `test_fbx_facts.py` says. Only
the FBX files this script changes on purpose are committed: each rerun stamps new times in them.
"""

import json
import shutil
import struct
import sys
import zlib
from pathlib import Path

try:
    import bpy
except ImportError:
    sys.exit("Run this with Blender: blender -b --factory-startup -P make_fixtures.py")

LIBRARY = Path(__file__).resolve().parent / "library"


def write(path: str, data: bytes | str) -> Path:
    target = LIBRARY / path
    target.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(data, str):
        data = data.encode()
    target.write_bytes(data)
    return target


def png(path: str, rgba: tuple[int, int, int, int], size: int = 4) -> None:
    """A `size` x `size` RGBA PNG of one colour, with no metadata chunk."""

    def chunk(kind: bytes, body: bytes) -> bytes:
        return (
            struct.pack(">I", len(body))
            + kind
            + body
            + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF)
        )

    rows = b"".join(b"\x00" + bytes(rgba) * size for _ in range(size))
    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    write(
        path,
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(rows, 9))
        + chunk(b"IEND", b""),
    )


def package(path: str, data: dict) -> None:
    write(path, json.dumps(data, indent=2) + "\n")


def conversion(path: str, steps: list[dict]) -> None:
    package(path, {"contractVersion": 1, "steps": steps})


def blender_image(path: str, rgba: tuple[float, float, float, float], file_format: str) -> None:
    target = LIBRARY / path
    target.parent.mkdir(parents=True, exist_ok=True)
    image = bpy.data.images.new(target.stem, 4, 4, alpha=True, float_buffer=file_format == "OPEN_EXR")
    image.pixels = list(rgba) * 16
    image.filepath_raw = str(target)
    image.file_format = file_format
    image.save()
    bpy.data.images.remove(image)


def fbx_cube(path: str, name: str) -> None:
    target = LIBRARY / path
    target.parent.mkdir(parents=True, exist_ok=True)
    bpy.ops.wm.read_homefile(use_empty=True)
    bpy.ops.mesh.primitive_cube_add(size=1)
    bpy.context.active_object.name = name
    bpy.ops.export_scene.fbx(filepath=str(target), use_selection=False)


def material(name: str, base_color: str | None = None, normal: str | None = None):
    """A new material `name`, with the library's images at `base_color` and `normal` wired to
    its Principled BSDF, so the FBX references them by type."""
    mat = bpy.data.materials.new(name)
    if mat.node_tree is None:
        mat.use_nodes = True
    nodes, links = mat.node_tree.nodes, mat.node_tree.links
    bsdf = next(n for n in nodes if n.type == "BSDF_PRINCIPLED")
    if base_color:
        tex = nodes.new("ShaderNodeTexImage")
        tex.image = bpy.data.images.load(str(LIBRARY / base_color))
        links.new(tex.outputs["Color"], bsdf.inputs["Base Color"])
    if normal:
        tex = nodes.new("ShaderNodeTexImage")
        tex.image = bpy.data.images.load(str(LIBRARY / normal))
        tex.image.colorspace_settings.name = "Non-Color"
        normal_map = nodes.new("ShaderNodeNormalMap")
        links.new(tex.outputs["Color"], normal_map.inputs["Color"])
        links.new(normal_map.outputs["Normal"], bsdf.inputs["Normal"])
    return mat


def box(name: str, size=(1, 1, 1), location=(0, 0, 0), materials=()):
    """A box mesh object `name` of `size`, its faces taking `materials` in turn."""
    bpy.ops.mesh.primitive_cube_add(size=1, location=location)
    obj = bpy.context.active_object
    obj.name = obj.data.name = name
    obj.scale = size
    bpy.ops.object.transform_apply(scale=True)
    for mat in materials:
        obj.data.materials.append(mat)
    for i, polygon in enumerate(obj.data.polygons):
        polygon.material_index = i % max(1, len(materials))
    return obj


def export_fbx(path: str, **settings) -> Path:
    """Exports the scene to `path`. Each texture is named by its path relative to the FBX, and
    by its absolute path on the machine that made it, as real FBX files are."""
    target = LIBRARY / path
    target.parent.mkdir(parents=True, exist_ok=True)
    bpy.ops.export_scene.fbx(
        filepath=str(target), use_selection=False, path_mode="RELATIVE", **settings
    )
    return target


def renamed_material(target: Path, old: str, new: str) -> None:
    """Renames the FBX's material `old` to `new`, of the same length, in place, so it can hold
    two materials of one name, which Blender itself never makes."""
    assert len(old) == len(new)
    old_name, new_name = f"{old}\x00\x01Material".encode(), f"{new}\x00\x01Material".encode()
    data = target.read_bytes()
    assert data.count(old_name) == 1
    target.write_bytes(data.replace(old_name, new_name))


def skinned(name: str, bones: list[str], take: str, mat) -> None:
    """A mesh `name` skinned to an armature with a chain of `bones`, and one action `take`
    turning the second bone."""
    bpy.ops.object.armature_add(location=(0, 0, 0))
    armature = bpy.context.active_object
    armature.name = armature.data.name = "Armature"
    bpy.ops.object.mode_set(mode="EDIT")
    edit_bones = armature.data.edit_bones
    edit_bones.remove(edit_bones[0])
    parent = None
    for i, bone_name in enumerate(bones):
        bone = edit_bones.new(bone_name)
        bone.head, bone.tail = (0, 0, i * 0.6), (0, 0, (i + 1) * 0.6)
        bone.parent = parent
        parent = bone
    bpy.ops.object.mode_set(mode="OBJECT")

    height = 0.6 * len(bones)
    mesh = box(name, size=(0.5, 0.3, height), location=(0, 0, height / 2), materials=[mat])
    # The bottom vertices follow the first bone and the top ones the last
    for bone_name, low in [(bones[0], True), (bones[-1], False)]:
        group = mesh.vertex_groups.new(name=bone_name)
        group.add([v.index for v in mesh.data.vertices if (v.co.z < 0) == low], 1.0, "REPLACE")
    mesh.parent = armature
    mesh.modifiers.new("Armature", "ARMATURE").object = armature

    action = bpy.data.actions.new(take)
    armature.animation_data_create().action = action
    bone = armature.pose.bones[bones[1]]
    bone.rotation_mode = "XYZ"
    for frame, angle in [(1, 0.0), (10, 0.6), (20, 0.0)]:
        bone.rotation_euler = (angle, 0, 0)
        bone.keyframe_insert("rotation_euler", frame=frame)
    scene = bpy.context.scene
    scene.frame_start, scene.frame_end = 1, 20
    # Without all actions, Blender writes the scene's animation as one take named after it
    scene.name = take


def models() -> None:
    """The FBX models `fbx_facts` and the Check read (step 4), in `Dungeon Props/Models/`."""
    models = "Dungeon Props/Models"
    floor = "Dungeon Props/Textures/T_StoneFloor_D.png"
    floor_normal = "Dungeon Props/Textures/T_StoneFloor_N.png"

    # Centimetres and Z up, off centre, with an asymmetric nose on its front (-Y): 40 x 30 x
    # 180 cm
    bpy.ops.wm.read_homefile(use_empty=True)
    bpy.context.scene.unit_settings.scale_length = 0.01
    stone = material("Stone", floor, floor_normal)
    body = box("SM_Totem", size=(40, 30, 180), location=(25, 10, 90), materials=[stone])
    nose = box("Nose", size=(10, 10, 10), location=(32, -10, 150), materials=[stone])
    bpy.ops.object.select_all(action="DESELECT")
    nose.select_set(True)
    body.select_set(True)
    bpy.context.view_layer.objects.active = body
    bpy.ops.object.join()
    export_fbx(f"{models}/SM_Totem.fbx", axis_up="Z", axis_forward="-Y", apply_unit_scale=True)

    # Unreal style: two LODs with two slots each, and a UCX_ hull with its own material
    bpy.ops.wm.read_homefile(use_empty=True)
    slots = [material("Stone", floor, floor_normal), material("Moss")]
    box("SM_Pillar_LOD0", size=(0.6, 0.6, 3), materials=slots)
    box("SM_Pillar_LOD1", size=(0.6, 0.6, 3), materials=slots)
    box("UCX_SM_Pillar_00", size=(0.7, 0.7, 3), materials=[material("Collision")])
    export_fbx(f"{models}/SM_Pillar.fbx")

    # The same, with a hull that has no material
    bpy.ops.wm.read_homefile(use_empty=True)
    iron = material("Iron")
    box("SM_Gate_LOD0", size=(2, 0.2, 2.5), materials=[iron])
    box("SM_Gate_LOD1", size=(2, 0.2, 2.5), materials=[iron])
    box("UCX_SM_Gate_00", size=(2, 0.3, 2.5))
    export_fbx(f"{models}/SM_Gate.fbx")

    # No material at all
    bpy.ops.wm.read_homefile(use_empty=True)
    box("SM_Boulder", size=(1.2, 1, 0.8))
    export_fbx(f"{models}/SM_Boulder.fbx")

    # Skinned, with one take
    bpy.ops.wm.read_homefile(use_empty=True)
    skinned("SK_Guard", ["Hips", "Spine", "Head"], "Wave", material("Cloth"))
    export_fbx(
        f"{models}/SK_Guard.fbx",
        bake_anim=True,
        bake_anim_use_all_actions=False,
        bake_anim_use_nla_strips=False,
    )

    # Only a UCX_ hull and no take: nothing to export
    bpy.ops.wm.read_homefile(use_empty=True)
    box("UCX_SM_Hull_00", materials=[material("Collision")])
    export_fbx(f"{models}/SM_Hull.fbx")

    # Its only material is named `Material`, as Blender's default cube's is
    bpy.ops.wm.read_homefile(use_empty=True)
    box("SM_Stool", size=(0.4, 0.4, 0.5), materials=[material("Material")])
    export_fbx(f"{models}/SM_Stool.fbx")

    # Two materials named Wood
    bpy.ops.wm.read_homefile(use_empty=True)
    box("SM_Bench_Seat", size=(1.5, 0.4, 0.1), location=(0, 0, 0.45), materials=[material("Wood")])
    box("SM_Bench_Legs", size=(1.4, 0.3, 0.4), location=(0, 0, 0.2), materials=[material("W00d")])
    renamed_material(export_fbx(f"{models}/SM_Bench.fbx"), "W00d", "Wood")

    # A real DefaultMaterial, beside a mesh with no material
    bpy.ops.wm.read_homefile(use_empty=True)
    box("SM_Lamp_Base", size=(0.3, 0.3, 0.1), materials=[material("DefaultMaterial")])
    box("SM_Lamp_Shade", size=(0.4, 0.4, 0.3), location=(0, 0, 0.5))
    export_fbx(f"{models}/SM_Lamp.fbx")

    # A kept slot named OpenDAMDefault, beside a mesh with no material
    bpy.ops.wm.read_homefile(use_empty=True)
    box("SM_Shelf_Frame", size=(1, 0.3, 2), materials=[material("OpenDAMDefault")])
    box("SM_Shelf_Board", size=(0.9, 0.28, 0.05), location=(0, 0, 1))
    export_fbx(f"{models}/SM_Shelf.fbx")


OBJ_TRIANGLE = "o Barrel\nv 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n"

KENNEY_LICENSE = """Crate Kit (1.0)

Created/distributed by Kenney (www.kenney.nl)

License: (Creative Commons Zero, CC0)
http://creativecommons.org/publicdomain/zero/1.0/
"""

MIT_LICENSE = """MIT License

Copyright (c) 2026 Moss Maker

Permission is hereby granted, free of charge, to any person obtaining a copy of these assets.
"""


def main() -> None:
    if LIBRARY.exists():
        shutil.rmtree(LIBRARY)

    package(
        "Dungeon Props/datapackage.json",
        {
            "name": "dungeon-props",
            "title": "Dungeon Props",
            "version": "1.2.0",
            "contributors": [{"title": "Stylarts", "roles": ["publisher"]}],
            "sources": [{"title": "Store page", "path": "https://example.com/dungeon-props"}],
            "licenses": [
                {
                    "name": "LicenseRef-Stylarts-EULA",
                    "title": "Stylarts Standard EULA",
                    "path": "https://example.com/stylarts-eula",
                }
            ],
        },
    )
    png("Dungeon Props/Textures/T_StoneFloor_D.png", (120, 110, 100, 255))
    png("Dungeon Props/Textures/T_StoneFloor_N.png", (128, 128, 255, 255))
    png("Dungeon Props/Textures/T_Banner_BaseColor.png", (160, 20, 20, 255))
    png("Dungeon Props/Textures/T_Rune_AO.png", (200, 200, 200, 255))
    png("Dungeon Props/Textures/mattress_01.png", (230, 220, 200, 255))
    # Each refused, by a build-order guard or for good
    png("Dungeon Props/Textures/T_StoneFloor_ORM.png", (255, 128, 0, 255))
    png("Dungeon Props/Textures/T_Wall_N_DX.png", (128, 128, 255, 255))
    blender_image("Dungeon Props/Textures/T_Wall_D.jpg", (0.4, 0.35, 0.3, 1.0), "JPEG")
    blender_image("Dungeon Props/Textures/T_Sky.exr", (0.2, 0.4, 0.8, 1.0), "OPEN_EXR")
    fbx_cube("Dungeon Props/Models/SM_Chest.fbx", "SM_Chest")
    # SM_Chest.obj joins the FBX in a format group; SM_Barrel.obj is a model with no FBX
    write("Dungeon Props/Models/SM_Chest.obj", OBJ_TRIANGLE.replace("Barrel", "Chest"))
    write("Dungeon Props/Models/SM_Barrel.obj", OBJ_TRIANGLE)
    write("Dungeon Props/Docs/notes.md", "# Notes\n\nNot an asset the game takes.\n")

    write("Kenney Crates/License.txt", KENNEY_LICENSE)
    png("Kenney Crates/crate_wood.png", (150, 100, 50, 255))
    png("Kenney Crates/banner.png", (20, 20, 160, 255))
    # A stem with no letter or digit gives no default name
    png("Kenney Crates/___.png", (60, 60, 60, 255))

    write("Unity Moss/License.txt", MIT_LICENSE)
    png("Unity Moss/T_Moss_D.png", (40, 120, 40, 255))
    write("Unity Moss/T_Moss_D.png.meta", "fileFormatVersion: 2\nguid: 0123456789abcdef0123456789abcdef\n")

    conversion(
        "Converted/conversion.json",
        [{"tool": "UE Asset Exporter", "toolVersion": "5.4", "from": "uasset"}],
    )
    package(
        "Converted/Ruins/datapackage.json",
        {"name": "ruins", "title": "Ruins", "licenses": [{"name": "CC-BY-4.0"}]},
    )
    png("Converted/Ruins/T_Pillar_D.png", (180, 170, 150, 255))
    # A normal map in an unreal pack is DirectX unless its name says otherwise (D14)
    png("Converted/Ruins/T_Pillar_N.png", (128, 127, 255, 255))

    for folder, name, colour in [
        ("Foo A", "foo", (10, 10, 10, 255)),
        ("Foo B", "foo", (20, 20, 20, 255)),
        ("Foo Two", "foo-2", (30, 30, 30, 255)),
    ]:
        package(f"{folder}/datapackage.json", {"name": name, "title": folder})
        png(f"{folder}/tile_{folder.split()[1].lower()}.png", colour)

    write("---/License.txt", "All rights reserved.\n")
    png("---/dash.png", (90, 90, 90, 255))

    png("Loose/stray.png", (255, 0, 255, 255))

    package(
        "Scans/datapackage.json",
        {"name": "scans", "title": "Scans", "licenses": [{"name": "CC0-1.0"}]},
    )
    conversion(
        "Scans/conversion.json",
        [
            {
                "tool": "Substance 3D Sampler",
                "toolVersion": "4.2",
                "from": "jpg",
                "settings": {"resolution": 2048},
            }
        ],
    )
    png("Scans/T_Boulder_D.png", (110, 105, 95, 255))
    # A deeper conversion.json replaces the pack's for everything below it
    conversion(
        "Scans/Cliffs/conversion.json",
        [
            {"tool": "RealityScan", "toolVersion": "2.0", "from": "jpg"},
            {
                "tool": "Substance 3D Sampler",
                "toolVersion": "4.2",
                "by": "delight",
                "from": "exr",
                "settings": {"resolution": 4096, "tiling": True},
            },
        ],
    )
    png("Scans/Cliffs/T_Cliff_D.png", (95, 90, 85, 255))

    models()


main()
