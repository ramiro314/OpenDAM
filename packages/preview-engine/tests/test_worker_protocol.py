"""Trimesh-branch previews (plan textured-fbx-previews, C6 and C11).

Every image over 1024 px in a loaded model's materials is downscaled before the GLB is written.
The glTF and OBJ+MTL sources are generated in `tmp_path`, and each test reads the GLB back.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import numpy as np
import trimesh
from PIL import Image
from test_fbx_scene import glb_images, read_glb
from trimesh.visual import TextureVisuals
from trimesh.visual.material import PBRMaterial

import textures
import worker

ENGINE = Path(__file__).resolve().parent.parent


def quad(material, x: float = 0.0) -> trimesh.Trimesh:
    mesh = trimesh.Trimesh(
        vertices=[[x, 0, 0], [x + 1, 0, 0], [x + 1, 1, 0], [x, 1, 0]],
        faces=[[0, 1, 2], [0, 2, 3]],
        process=False,
    )
    mesh.visual = TextureVisuals(uv=[[0, 0], [1, 0], [1, 1], [0, 1]], material=material)
    return mesh


def image_sizes(gltf: dict, binary: bytes) -> list[tuple[str, tuple[int, int]]]:
    return [(mime, img.size) for mime, img in glb_images(gltf, binary)]


def stored_images(gltf: dict) -> int:
    """How many distinct image blobs the GLB holds."""
    return len({image["bufferView"] for image in gltf.get("images", [])})


def write_gltf(tmp_path: Path) -> Path:
    """
    Two quads: one with a 2048 px base color, normal and metallic/roughness map, and one whose
    base color texture points at the same glTF image as the first, in another tint.
    """
    a = PBRMaterial(
        name="A",
        baseColorTexture=Image.new("RGB", (2048, 2048), (200, 50, 50)),
        normalTexture=Image.new("RGB", (2048, 2048), (128, 128, 255)),
        metallicRoughnessTexture=Image.new("RGB", (2048, 1024), (0, 128, 0)),
    )
    b = PBRMaterial(name="B", baseColorTexture=Image.new("RGB", (8, 8)), baseColorFactor=[0.5] * 4)
    files = trimesh.Scene([quad(a), quad(b, 2)]).export(file_type="gltf")
    tree = json.loads(files.pop("model.gltf"))
    by_name = {m["name"]: m for m in tree["materials"]}
    by_name["B"]["pbrMetallicRoughness"]["baseColorTexture"] = by_name["A"]["pbrMetallicRoughness"][
        "baseColorTexture"
    ]
    for name, data in files.items():
        (tmp_path / name).write_bytes(data)
    source = tmp_path / "model.gltf"
    source.write_text(json.dumps(tree))
    return source


def test_every_pbr_slot_over_1024_px_is_downscaled_and_a_shared_one_once(tmp_path, monkeypatch):
    source = write_gltf(tmp_path)
    calls = []
    shrink = textures.shrink_image
    monkeypatch.setattr(textures, "shrink_image", lambda img: calls.append(img) or shrink(img))

    res = worker.model_meta(str(source), str(tmp_path / "thumbs"), "42")

    assert len(calls) == 3
    gltf, binary = read_glb(res["preview_model"])
    by_name = {m["name"]: m for m in gltf["materials"]}
    a, b = by_name["A"], by_name["B"]
    slots = [
        a["pbrMetallicRoughness"]["baseColorTexture"],
        a["normalTexture"],
        a["pbrMetallicRoughness"]["metallicRoughnessTexture"],
    ]
    views = {gltf["images"][gltf["textures"][s["index"]]["source"]]["bufferView"] for s in slots}
    assert len(views) == 3
    assert sorted(image_sizes(gltf, binary)) == sorted(
        [("image/jpeg", (1024, 1024))] * 3 + [("image/jpeg", (1024, 512))]
    )
    # B's base color is A's: the GLB holds three images, not four.
    shared = gltf["textures"][b["pbrMetallicRoughness"]["baseColorTexture"]["index"]]["source"]
    assert gltf["images"][shared]["bufferView"] in views
    assert stored_images(gltf) == 3


def test_every_slot_is_checked_and_small_images_keep_their_encoding(tmp_path):
    big = Image.new("RGB", (2048, 2048))
    small = Image.new("RGB", (512, 512))
    simple = trimesh.visual.material.SimpleMaterial(image=Image.new("RGB", (4096, 1024)))
    pbr = PBRMaterial(**{slot: big for slot in worker.PBR_TEXTURE_SLOTS})
    kept = PBRMaterial(baseColorTexture=small)
    scene = trimesh.Scene([quad(simple), quad(pbr, 2), quad(kept, 4)])

    worker.downscale_textures(scene)

    assert simple.image.size == (1024, 256) and simple.image.format == "JPEG"
    shrunk = [getattr(pbr, slot) for slot in worker.PBR_TEXTURE_SLOTS]
    assert len({id(img) for img in shrunk}) == 1 and shrunk[0].size == (1024, 1024)
    assert kept.baseColorTexture is small


def test_an_image_with_any_alpha_below_255_stays_png():
    opaque = Image.new("RGBA", (2048, 2048), (10, 20, 30, 255))
    faint = opaque.copy()
    faint.putpixel((0, 0), (10, 20, 30, 254))

    assert textures.shrink_image(opaque).format == "JPEG"
    shrunk = textures.shrink_image(faint)
    assert shrunk.mode == "RGBA" and shrunk.size == (1024, 1024)


def write_obj(tmp_path: Path, name: str, texture: Image.Image) -> Path:
    """A textured quad in `<name>.obj`, its `<name>.mtl` and its `<name>.png` map."""
    texture.save(tmp_path / f"{name}.png")
    (tmp_path / f"{name}.mtl").write_text(f"newmtl Mat\nKd 1 1 1\nmap_Kd {name}.png\n")
    (tmp_path / f"{name}.obj").write_text(
        f"mtllib {name}.mtl\n"
        "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\n"
        "vt 0 0\nvt 1 0\nvt 1 1\nvt 0 1\n"
        "usemtl Mat\nf 1/1 2/2 3/3\nf 1/1 3/3 4/4\n"
    )
    return tmp_path / f"{name}.obj"


def test_model_meta_over_stdio_downscales_an_obj_texture(tmp_path):
    """One worker process serves both requests over stdio, as the app drives it."""
    opaque = write_obj(tmp_path, "opaque", Image.new("RGB", (2048, 2048), (90, 140, 60)))
    alpha = np.full((2048, 2048), 255, dtype=np.uint8)
    alpha[:, :1024] = 0
    cutout = Image.new("RGBA", (2048, 2048), (90, 140, 60, 255))
    cutout.putalpha(Image.fromarray(alpha))
    with_alpha = write_obj(tmp_path, "alpha", cutout)

    requests = [
        {
            "id": n,
            "method": "model_meta",
            "params": {
                "source": str(src),
                "output_dir": str(tmp_path / "thumbs"),
                "name": src.stem,
            },
        }
        for n, src in enumerate([opaque, with_alpha], 1)
    ]
    run = subprocess.run(
        [sys.executable, "worker.py"],
        cwd=ENGINE,
        input="".join(json.dumps(r) + "\n" for r in requests),
        capture_output=True,
        text=True,
        timeout=120,
    )

    replies = [json.loads(line) for line in run.stdout.splitlines()]
    assert [r["id"] for r in replies] == [1, 2], run.stderr
    expected = {1: ("image/jpeg", (1024, 1024)), 2: ("image/png", (1024, 1024))}
    for reply in replies:
        result = reply["result"]
        assert result["ok"], result
        gltf, binary = read_glb(result["preview_model"])
        assert image_sizes(gltf, binary) == [expected[reply["id"]]]
