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
  (origin unreal, so its normal map is DirectX, and its textures refused until conversion chains
  are built).
- `Foo A/`, `Foo B/` and `Foo Two/`: packs named `foo`, `foo` and `foo-2`.
- `---/`: a pack whose folder slugifies to nothing and whose license names none (`Unknown`).
- `Loose/stray.png`: an image in no pack.
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

    write(
        "Converted/conversion.json",
        json.dumps(
            {
                "contractVersion": 1,
                "steps": [{"tool": "UE Asset Exporter", "toolVersion": "5.4", "from": "uasset"}],
            },
            indent=2,
        )
        + "\n",
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


main()
