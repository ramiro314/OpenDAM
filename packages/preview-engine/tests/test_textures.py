"""Texture lookup and loading for FBX/3DS previews (plan textured-fbx-previews, C3, C5 and C7).

Folder trees and images are generated in `tmp_path`. A library lives in `tmp_path / "lib"`, so
`tmp_path` itself is outside it.
"""

from __future__ import annotations

import struct
from pathlib import Path

import pytest
from PIL import Image

import textures
from textures import Bounds, bounds, load_texture, resolve_texture


@pytest.fixture(autouse=True)
def fresh_index_cache():
    textures._INDEXES.clear()
    yield
    textures._INDEXES.clear()


def touch(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"")
    return path


def model(lib: Path, rel: str) -> str:
    return str(touch(lib / rel))


def resolve(reference, source, library_root, pack_root=None):
    """`(path relative to the library, via)`, or None."""
    found = resolve_texture(reference, source, bounds(source, library_root, pack_root))
    if found is None:
        return None
    path, via = found
    return path.relative_to(Path(library_root).resolve()).as_posix(), via


@pytest.fixture
def lib(tmp_path) -> Path:
    (tmp_path / "lib").mkdir()
    return tmp_path / "lib"


# ── Step 1: as given ──────────────────────────────────────────────────────


def test_an_absolute_path_inside_the_root_is_taken_as_given(lib):
    src = model(lib, "Pack/Meshes/rock.fbx")
    tex = touch(lib / "Pack/Shared/stone.png")
    assert resolve(str(tex), src, lib, lib / "Pack") == ("Pack/Shared/stone.png", "given")


def test_an_absolute_path_outside_the_root_is_ignored(lib, tmp_path):
    src = model(lib, "Pack/Meshes/rock.fbx")
    outside = touch(tmp_path / "elsewhere/stone.png")
    assert resolve(str(outside), src, lib, lib / "Pack") is None


# ── Step 2: relative ──────────────────────────────────────────────────────


def test_a_relative_path_matches_ignoring_case_with_backslashes(lib):
    src = model(lib, "Pack/Meshes/rock.fbx")
    touch(lib / "Pack/textures/T_Rock_BC.PNG")
    assert resolve("..\\Textures\\T_Rock_BC.png", src, lib, lib / "Pack") == (
        "Pack/textures/T_Rock_BC.PNG",
        "relative",
    )


def test_dot_dot_out_of_the_library_root_is_ignored(lib, tmp_path):
    src = model(lib, "Pack/rock.fbx")
    touch(tmp_path / "outside/stone.png")
    assert resolve("../../outside/stone.png", src, lib) is None


def test_a_symlink_out_of_the_root_is_never_followed(lib, tmp_path):
    src = model(lib, "Pack/Meshes/rock.fbx")
    secret = touch(tmp_path / "outside/stone.png")
    (lib / "Pack/Textures").mkdir(parents=True)
    (lib / "Pack/Textures/stone.png").symlink_to(secret)
    assert resolve("..\\Textures\\stone.png", src, lib, lib / "Pack") is None


# ── Step 3: by name ───────────────────────────────────────────────────────


def test_by_name_in_the_models_folder(lib):
    src = model(lib, "Pack/Meshes/rock.fbx")
    touch(lib / "Pack/Meshes/Stone.png")
    assert resolve("U:\\Dropbox\\Art\\stone.png", src, lib, lib / "Pack") == (
        "Pack/Meshes/Stone.png",
        "name",
    )


def test_by_name_in_a_textures_folder_one_level_up(lib):
    src = model(lib, "Pack/Meshes/rock.fbx")
    touch(lib / "Pack/Textures/Rocks/stone.png")
    assert resolve("U:\\Art\\stone.png", src, lib, lib / "Pack") == (
        "Pack/Textures/Rocks/stone.png",
        "name",
    )


def test_by_name_only_in_the_whole_pack(lib):
    src = model(lib, "Pack/Meshes/rock.fbx")
    touch(lib / "Pack/Art/Deep/stone.png")
    assert resolve("stone.png", src, lib, lib / "Pack") == ("Pack/Art/Deep/stone.png", "name")
    # Without the pack, the whole-pack step doesn't run.
    assert resolve("stone.png", src, lib) is None


def test_the_nearest_tier_wins(lib):
    src = model(lib, "Pack/Meshes/rock.fbx")
    touch(lib / "Pack/Textures/stone.png")
    touch(lib / "Pack/Meshes/Textures/stone.png")
    assert resolve("stone.png", src, lib, lib / "Pack")[0] == "Pack/Meshes/Textures/stone.png"


def test_built_in_and_urp_copies(lib):
    # A model in the URP tree takes the URP copy, which its walk up reaches first.
    src = model(lib, "Pack/URP/Models/rock.fbx")
    touch(lib / "Pack/Built-In/Textures/stone.png")
    touch(lib / "Pack/URP/Textures/stone.png")
    assert resolve("stone.png", src, lib, lib / "Pack")[0] == "Pack/URP/Textures/stone.png"
    # A model outside both reaches them only in the whole pack, at the same distance:
    # the path ignoring case breaks the tie.
    src = model(lib, "Pack/Models/rock.fbx")
    assert resolve("stone.png", src, lib, lib / "Pack")[0] == "Pack/Built-In/Textures/stone.png"


def test_previews_and_other_render_folders_are_never_searched(lib):
    src = model(lib, "Pack/rock.fbx")
    for folder in ("Previews", "preview", "Renders", "Isometric", "Sprites", ".opendam/thumbs"):
        touch(lib / "Pack" / folder / "rock.png")
    assert resolve("rock.png", src, lib, lib / "Pack") is None
    touch(lib / "Pack/Textures/rock.jpg")
    textures._INDEXES.clear()
    assert resolve("rock.png", src, lib, lib / "Pack")[0] == "Pack/Textures/rock.jpg"


def test_not_found(lib):
    src = model(lib, "Pack/Meshes/rock.fbx")
    touch(lib / "Pack/Textures/other.png")
    assert resolve("..\\Textures\\stone.png", src, lib, lib / "Pack") is None


def test_without_a_pack_the_search_goes_at_most_two_levels_up(lib):
    src = model(lib, "a/b/c/rock.fbx")
    touch(lib / "textures/far.png")  # three levels up
    touch(lib / "a/Textures/near.png")  # two levels up
    assert resolve("near.png", src, lib) == ("a/Textures/near.png", "name")
    assert resolve("far.png", src, lib) is None


def test_the_walk_up_never_leaves_the_library_root(lib, tmp_path):
    src = model(lib, "rock.fbx")
    touch(tmp_path / "Textures/stone.png")
    assert resolve("stone.png", src, lib) is None


def test_a_capped_index_is_searched_as_far_as_it_got(lib, monkeypatch):
    monkeypatch.setattr(textures, "INDEX_CAP", 1)
    src = model(lib, "Pack/rock.fbx")
    touch(lib / "Pack/a/first.png")
    touch(lib / "Pack/b/second.png")
    assert resolve("first.png", src, lib, lib / "Pack")[0] == "Pack/a/first.png"
    assert resolve("second.png", src, lib, lib / "Pack") is None


def test_the_index_is_cached_per_root_for_five_minutes(lib, monkeypatch):
    clock = [1000.0]
    monkeypatch.setattr(textures.time, "monotonic", lambda: clock[0])
    src = model(lib, "Pack/rock.fbx")
    assert resolve("late.png", src, lib, lib / "Pack") is None
    touch(lib / "Pack/Textures/late.png")
    assert resolve("late.png", src, lib, lib / "Pack") is None
    clock[0] += textures.INDEX_TTL
    assert resolve("late.png", src, lib, lib / "Pack")[0] == "Pack/Textures/late.png"


def test_the_index_cache_keeps_four_roots(lib):
    for n in range(6):
        src = model(lib, f"Pack{n}/rock.fbx")
        resolve("x.png", src, lib, lib / f"Pack{n}")
    assert len(textures._INDEXES) == textures.INDEX_ROOTS
    assert [key[0].name for key in textures._INDEXES] == ["Pack2", "Pack3", "Pack4", "Pack5"]


# ── Extensions and PSD precedence ─────────────────────────────────────────


def test_another_extension_is_found(lib):
    src = model(lib, "Pack/rock.fbx")
    touch(lib / "Pack/Textures/stone.png")
    assert resolve("..\\Textures\\stone.tif", src, lib, lib / "Pack")[0] == (
        "Pack/Textures/stone.png"
    )


def test_the_exact_extension_comes_first_within_a_tier(lib):
    src = model(lib, "Pack/rock.fbx")
    touch(lib / "Pack/Textures/stone.png")
    touch(lib / "Pack/Textures/stone.tga")
    assert resolve("C:\\stone.tga", src, lib, lib / "Pack")[0] == "Pack/Textures/stone.tga"
    assert resolve("C:\\stone.bmp", src, lib, lib / "Pack")[0] == "Pack/Textures/stone.png"


def test_psd_comes_last(lib):
    src = model(lib, "Pack/rock.fbx")
    touch(lib / "Pack/Textures/stone.psd")
    touch(lib / "Pack/Textures/stone.webp")
    assert resolve("C:\\stone.psd", src, lib, lib / "Pack")[0] == "Pack/Textures/stone.webp"


def test_a_psd_reference_takes_a_png_sibling(lib):
    src = model(lib, "Pack/Meshes/rock.fbx")
    touch(lib / "Pack/Textures/X.psd")
    assert resolve("..\\Textures\\X.psd", src, lib, lib / "Pack") == (
        "Pack/Textures/X.psd",
        "relative",
    )
    touch(lib / "Pack/Textures/X.png")
    textures._INDEXES.clear()
    assert resolve("..\\Textures\\X.psd", src, lib, lib / "Pack") == (
        "Pack/Textures/X.png",
        "name",
    )


def test_a_non_psd_anywhere_beats_a_psd_found_literally(lib):
    src = model(lib, "Pack/Meshes/rock.fbx")
    touch(lib / "Pack/Meshes/X.psd")
    touch(lib / "Pack/Art/Far/X.jpg")
    assert resolve("X.psd", src, lib, lib / "Pack")[0] == "Pack/Art/Far/X.jpg"


def test_a_nearer_other_extension_beats_the_exact_one_further_away(lib):
    src = model(lib, "Pack/Meshes/rock.fbx")
    touch(lib / "Pack/Meshes/stone.png")
    touch(lib / "Pack/Art/stone.tga")
    assert resolve("C:\\stone.tga", src, lib, lib / "Pack")[0] == "Pack/Meshes/stone.png"


# ── Roots ─────────────────────────────────────────────────────────────────


def test_without_a_library_root_nothing_is_looked_up(lib, monkeypatch):
    src = model(lib, "Pack/rock.fbx")
    touch(lib / "Pack/stone.png")
    assert bounds(src, None, None) is None
    assert bounds(src, "", str(lib / "Pack")) is None
    monkeypatch.setattr(textures.os, "walk", pytest.fail)
    monkeypatch.setattr(textures.os, "scandir", pytest.fail)
    assert resolve_texture("stone.png", src, None) is None


def test_a_library_root_that_does_not_hold_the_model_counts_as_none(lib, tmp_path):
    src = model(lib, "Pack/rock.fbx")
    (tmp_path / "other").mkdir()
    assert bounds(src, str(tmp_path / "other"), None) is None


def test_a_pack_root_outside_the_library_root_is_ignored(lib, tmp_path):
    src = model(lib, "a/b/c/rock.fbx")
    touch(lib / "z/q/stone.png")
    assert bounds(src, str(lib), str(tmp_path)) == Bounds(lib.resolve(), None)
    assert resolve("stone.png", src, lib, tmp_path) is None
    assert resolve("stone.png", src, lib, lib) == ("z/q/stone.png", "name")


def test_an_embedded_reference_finds_nothing(lib):
    src = model(lib, "Pack/rock.fbx")
    touch(lib / "Pack/0.png")
    assert resolve("*0", src, lib, lib / "Pack") is None


# ── Loading ───────────────────────────────────────────────────────────────


def save(img: Image.Image, path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    img.save(path)
    return path


def rgba(size, alpha) -> Image.Image:
    """An RGBA image whose alpha channel is `alpha`, an `L` image or a constant."""
    img = Image.new("RGBA", size, (120, 160, 80, 255))
    img.putalpha(alpha)
    return img


def write_psd(path: Path, size: tuple[int, int], color: tuple[int, int, int]) -> Path:
    """A minimal RGB PSD holding only its flattened composite, uncompressed."""
    w, h = size
    header = b"8BPS" + struct.pack(">H6xHIIHH", 1, 3, h, w, 8, 3)
    sections = struct.pack(">III", 0, 0, 0)  # color mode data, resources, layers
    planes = b"".join(bytes([c]) * (w * h) for c in color)
    path.write_bytes(header + sections + struct.pack(">H", 0) + planes)
    return path


def test_an_opaque_texture_is_downscaled_and_reencoded_as_jpeg(tmp_path):
    tex = load_texture(save(Image.new("RGB", (2048, 1024), (200, 10, 10)), tmp_path / "a.png"))
    assert (tex.status, tex.alpha) == ("ok", "none")
    assert tex.image.format == "JPEG"
    assert tex.image.size == (1024, 512)


def test_a_tall_texture_is_downscaled_on_its_longest_side(tmp_path):
    tex = load_texture(save(Image.new("RGB", (512, 2048)), tmp_path / "a.tga"))
    assert tex.image.size == (256, 1024)


def test_a_small_texture_keeps_its_size_and_becomes_jpeg(tmp_path):
    tex = load_texture(save(Image.new("RGB", (64, 32)), tmp_path / "a.bmp"))
    assert (tex.image.format, tex.image.size) == ("JPEG", (64, 32))


def test_a_psd_is_read_as_its_composite(tmp_path):
    tex = load_texture(write_psd(tmp_path / "a.psd", (8, 4), (10, 200, 30)))
    assert tex.status == "ok"
    assert tex.image.size == (8, 4)
    r, g, b = tex.image.getpixel((4, 2))
    assert abs(r - 10) < 8 and abs(g - 200) < 8 and abs(b - 30) < 8


def test_an_unreadable_texture(tmp_path):
    (tmp_path / "bad.png").write_bytes(b"\x89PNG not really")
    truncated = save(Image.new("RGB", (256, 256), (9, 9, 9)), tmp_path / "cut.png")
    truncated.write_bytes(truncated.read_bytes()[:100])
    assert load_texture(tmp_path / "bad.png").status == "unreadable"
    assert load_texture(truncated).status == "unreadable"
    assert load_texture(tmp_path / "gone.png").status == "unreadable"


def test_a_texture_over_8192_px_is_too_large_without_decoding(tmp_path, monkeypatch):
    path = save(Image.new("L", (8193, 1)), tmp_path / "huge.png")
    monkeypatch.setattr(Image.Image, "load", pytest.fail)
    assert load_texture(path).status == "too_large"
    monkeypatch.undo()
    assert load_texture(save(Image.new("L", (8192, 1)), tmp_path / "edge.png")).status == "ok"


def test_all_zero_alpha_is_ignored_and_dropped(tmp_path):
    tex = load_texture(save(rgba((64, 64), 0), tmp_path / "zero.tga"))
    assert tex.alpha == "ignored"
    assert tex.image.format == "JPEG"
    assert tex.image.mode == "RGB"


def test_smoothness_like_alpha_is_ignored(tmp_path):
    gradient = Image.linear_gradient("L").resize((64, 64))
    tex = load_texture(save(rgba((64, 64), gradient), tmp_path / "smooth.png"))
    assert tex.alpha == "ignored"
    assert tex.image.format == "JPEG"


def test_binary_foliage_alpha_is_a_cutout_kept_as_png(tmp_path):
    alpha = Image.new("L", (64, 64), 0)
    alpha.paste(255, (16, 0, 48, 64))
    tex = load_texture(save(rgba((64, 64), alpha), tmp_path / "leaf.png"))
    assert tex.alpha == "cutout"
    assert tex.image.mode == "RGBA"
    assert tex.image.format != "JPEG"


def test_opaque_alpha_counts_as_none(tmp_path):
    tex = load_texture(save(rgba((32, 32), 255), tmp_path / "solid.png"))
    assert tex.alpha == "none"
    assert tex.image.format == "JPEG"


def test_a_thin_blade_atlas_is_classified_before_the_resize(tmp_path):
    # A 2048 px atlas of binary blades 2 px wide, offset so each straddles two output pixels.
    alpha = Image.new("L", (2048, 2048), 0)
    for x in range(1, 2048, 6):
        alpha.paste(255, (x, 0, x + 2, 2048))
    source = rgba((2048, 2048), alpha)

    # Classified at 1024 px, the blades read as mid-range alpha and the atlas as opaque.
    shrunk = alpha.resize((1024, 1024), Image.LANCZOS)
    hist = shrunk.histogram()
    assert sum(hist[16:240]) > 0.25 * 1024 * 1024
    assert textures.classify_alpha(shrunk) == "ignored"

    tex = load_texture(save(source, tmp_path / "grass.png"))
    assert tex.alpha == "cutout"
    assert tex.image.mode == "RGBA"
    assert tex.image.size == (1024, 1024)


def test_a_shared_texture_is_decoded_once_per_request(tmp_path, monkeypatch):
    path = save(Image.new("RGB", (16, 16)), tmp_path / "atlas.png")
    calls = []
    real = textures._load
    monkeypatch.setattr(textures, "_load", lambda p: calls.append(p) or real(p))
    cache = {}
    first = load_texture(path, cache)
    assert load_texture(path, cache) is first
    assert calls == [path]


# ── Report ────────────────────────────────────────────────────────────────


def test_the_report_counts_each_outcome(lib):
    src = model(lib, "Pack/Meshes/rock.fbx")
    save(Image.new("RGB", (8, 8)), lib / "Pack/Textures/stone.png")
    save(rgba((8, 8), 0), lib / "Pack/Textures/zero.tga")
    (lib / "Pack/Textures/bad.png").write_bytes(b"nope")
    save(Image.new("L", (8193, 1)), lib / "Pack/Textures/huge.png")
    scene = textures.SceneTextures(src, str(lib), str(lib / "Pack"))
    materials = [
        {"NAME": "Stone", "TEXTURES": {1: ["..\\Textures\\stone.png"]}},
        {"NAME": "Zero", "TEXTURES": {12: ["C:\\zero.tga"]}},
        {"NAME": "Bad", "TEXTURES": {1: ["bad.png"]}},
        {"NAME": "Huge", "TEXTURES": {1: ["huge.png"]}},
        {"NAME": "Gone", "TEXTURES": {1: ["..\\Textures\\T_Fill_B.png"], 6: ["stone.png"]}},
        {"NAME": "Embedded", "TEXTURES": {1: ["*0"]}},
        {"NAME": "Plain", "TEXTURES": {6: ["stone.png"]}},
    ]
    loaded = [scene.texture(m) for m in materials]

    assert [t is not None for t in loaded] == [True, True, False, False, False, False, False]
    assert scene.report == {
        "materials": 7,
        "textured": 2,
        "via": {"relative": 1, "name": 1},
        "missing": ["T_Fill_B", "*0", "material:Plain"],
        "unreadable": ["bad.png"],
        "too_large": ["huge.png"],
        "no_uv": 0,
        "alpha": {"cutout": 0, "ignored": 1, "none": 1},
        "drawn_nodes": 0,
        "skipped_nodes": 0,
        "no_root": False,
    }


def test_the_report_lists_at_most_ten_missing_names(lib):
    src = model(lib, "rock.fbx")
    scene = textures.SceneTextures(src, str(lib), None)
    for n in range(12):
        scene.texture({"TEXTURES": {1: [f"t{n}.png"]}})
    assert scene.report["missing"] == [f"t{n}" for n in range(10)]


def test_without_a_root_the_report_says_so_and_reads_nothing(lib, monkeypatch):
    src = model(lib, "rock.fbx")
    save(Image.new("RGB", (8, 8)), lib / "stone.png")
    monkeypatch.setattr(textures, "load_texture", pytest.fail)
    scene = textures.SceneTextures(src, None, None)
    assert scene.texture({"TEXTURES": {1: ["stone.png"]}}) is None
    assert scene.report["no_root"] is True
    assert scene.report["missing"] == []
