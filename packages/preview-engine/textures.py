"""
Texture lookup and loading for FBX/3DS previews (plan textured-fbx-previews, C3 and C5).

`resolve_texture` finds the file a material's texture reference names, or failing that the one
its naming points at, reading nothing outside the library root, and `load_texture` turns it into
an image for the preview GLB: at most 1024 px, JPEG unless its alpha looks like a cutout.
"""

from __future__ import annotations

import functools
import io
import os
import re
import time
from collections import OrderedDict
from dataclasses import dataclass
from pathlib import Path

# Lookup order of texture extensions. A PSD is a last resort across the whole lookup.
EXTENSIONS = ("png", "jpg", "jpeg", "tga", "tif", "tiff", "bmp", "webp", "psd")
# Folders the by-name search never looks in: renders, sprite sheets and the app's own data.
EXCLUDED_DIRS = frozenset({"previews", "preview", "renders", "isometric", "sprites", ".opendam"})
# Subfolders searched recursively at each level of the walk up from the model.
TEXTURE_DIRS = frozenset({"textures", "materials", "maps"})
# Without a pack, the by-name search walks up at most this many levels from the model's folder.
NO_PACK_LEVELS = 2

MAX_SIDE = 1024  # longest side of a texture in the GLB
MAX_SOURCE_SIDE = 8192  # a larger source is skipped as `too_large`, bounding one decode
JPEG_QUALITY = 85

# File index cache. Each index is valid 5 minutes, and its walk stops after 50,000 entries,
# folders and files of any kind. At most 4 whole packs are kept, and 64 levels of the walk up
# from a model (a folder's own files and its texture subfolders), which cost far less.
INDEX_ROOTS = 4
INDEX_LEVELS = 64
INDEX_TTL = 300.0
INDEX_CAP = 50_000


@dataclass(frozen=True)
class Bounds:
    """Where a lookup may read: the library root, and the model's pack inside it, if any."""

    library_root: Path
    pack_root: Path | None


def bounds(source: str, library_root: str | None, pack_root: str | None) -> Bounds | None:
    """
    The lookup bounds for the model at `source`, or None when there is no library root that
    contains it, in which case no texture file may be read. A pack root outside the library
    root, or not containing the model, is ignored. Paths are compared with symlinks resolved.
    """
    if not library_root:
        return None
    root = Path(os.path.realpath(library_root))
    model = Path(os.path.realpath(source))
    if not _inside(model, root):
        return None
    pack = None
    if pack_root:
        candidate = Path(os.path.realpath(pack_root))
        if _inside(candidate, root) and _inside(model, candidate):
            pack = candidate
    return Bounds(root, pack)


def _inside(path: Path, root: Path) -> bool:
    return path == root or root in path.parents


def _ext(name: str) -> str:
    return name.rsplit(".", 1)[1].lower() if "." in name else ""


def _is_psd(path: Path) -> bool:
    return _ext(path.name) == "psd"


def reference_parts(reference: str) -> list[str]:
    """A reference's path components, with `\\` read as `/`."""
    return [p for p in reference.replace("\\", "/").split("/") if p]


def resolve_texture(
    reference: str | None,
    source: str,
    where: Bounds | None,
    material_name: str | None = None,
    single_material: bool = False,
    files_cache: dict | None = None,
) -> tuple[Path, str] | None:
    """
    The file a material's texture reference names, as `(path, via)`, or None.

    `via` is the step that found it: `given` (an absolute path), `relative` (the path joined
    to the model's folder, each component ignoring case) or `name` (the file name, ignoring
    case and extension, nearest first). A non-PSD file found by any of these literal steps
    beats every PSD, and the first PSD they find beats the heuristics. Only when they find
    nothing, or there is no reference, do the heuristics run (`_heuristics`), again preferring
    any non-PSD match to a PSD. An embedded (`*N`) reference counts as none. Without a root
    (`where` is None) nothing is found. `files_cache` is `_heuristics`' per-request cache.
    """
    if where is None:
        return None
    model_dir = Path(os.path.realpath(source)).parent
    if reference and reference.startswith("*"):
        reference = None
    if reference:
        found = _first(_candidates(reference, model_dir, where))
        if found:
            return found
    return _first(
        _heuristics(
            reference, material_name, single_material, source, model_dir, where, files_cache
        )
    )


def _first(found) -> tuple[Path, str] | None:
    """The first non-PSD file of `found`, else its first PSD."""
    held = None
    for item in found:
        if not _is_psd(item[0]):
            return item
        held = held or item
    return held


def _candidates(reference: str, model_dir: Path, where: Bounds):
    """Every file the reference may name, in lookup order (C3 steps 1-3)."""
    root = where.library_root
    unix = reference.replace("\\", "/")

    # 1. As given.
    if unix.startswith("/"):
        path = _real_file(Path(unix), root)
        if path:
            yield path, "given"

    # 2. Relative to the model, each component ignoring case.
    else:
        path = _relative(model_dir, reference_parts(reference), root)
        if path:
            yield path, "relative"

    # 3. By name.
    parts = reference_parts(reference)
    if not parts:
        return
    name = parts[-1]
    stem = name.rsplit(".", 1)[0].lower() if "." in name else name.lower()
    for path in _by_name(stem, _ext(name), model_dir, where):
        yield path, "name"


def _real_file(path: Path, root: Path) -> Path | None:
    """`path` with symlinks resolved, if it is a file inside `root`. Nothing outside is touched."""
    if not _inside(Path(os.path.normpath(path)), root):
        return None
    real = Path(os.path.realpath(path))
    return real if _inside(real, root) and real.is_file() else None


def _relative(model_dir: Path, parts: list[str], root: Path) -> Path | None:
    """`parts` joined to `model_dir`, each component matched ignoring case, inside `root`."""
    current = model_dir
    for part in parts:
        if part == ".":
            continue
        if part == "..":
            current = current.parent
        else:
            exact = current / part
            if not exact.exists():
                try:
                    match = sorted(
                        e.name for e in os.scandir(current) if e.name.lower() == part.lower()
                    )
                except OSError:
                    return None
                if not match:
                    return None
                exact = current / match[0]
            current = exact
        # Never step outside the root, even on the way to a path back inside it.
        current = Path(os.path.realpath(current))
        if not _inside(current, root):
            return None
    return current if current != model_dir and current.is_file() else None


class _Tiers:
    """
    Where the by-name search and the heuristics look, tier by tier. A tier is a level of the
    walk up from the model's folder: that folder and its texture subfolders, recursively.
    Then the whole pack. Without a pack the walk goes at most `NO_PACK_LEVELS` up and there is
    no whole-pack tier. Paths are folder parts relative to `top`, the walk's last level.
    """

    def __init__(self, model_dir: Path, where: Bounds):
        levels = [model_dir]
        if where.pack_root:
            while levels[-1] != where.pack_root:
                levels.append(levels[-1].parent)
        else:
            while len(levels) <= NO_PACK_LEVELS and levels[-1] != where.library_root:
                levels.append(levels[-1].parent)
        self.levels = levels
        self.top = levels[-1]
        self.model_rel = model_dir.relative_to(self.top).parts
        self.where = where

    def indexes(self):
        """
        Each tier's index, nearest first, as `(folder parts from top, index)`. A tier is
        indexed only when the caller reaches it.
        """
        root = self.where.library_root
        for level in self.levels:
            yield level.relative_to(self.top).parts, _index(level, root, recursive=False)
        if self.where.pack_root:
            yield (), _index(self.top, root, recursive=True)

    def key(self, ext: str):
        """
        The order within a tier: the reference's own extension `ext` first unless it is a
        PSD, then the others in `EXTENSIONS` order, then the nearest folder, then the path
        ignoring case.
        """
        model_rel = self.model_rel

        def key(folder: tuple[str, ...], name: str):
            e = _ext(name)
            order = 0 if e == ext and ext != "psd" else 1 + EXTENSIONS.index(e)
            common = 0
            while common < min(len(folder), len(model_rel)) and folder[common] == model_rel[common]:
                common += 1
            distance = len(folder) + len(model_rel) - 2 * common
            return order, distance, "/".join((*folder, name)).lower()

        return key

    def path(self, folder: tuple[str, ...], name: str) -> Path:
        return self.top.joinpath(*folder, name)


def _by_name(stem: str, ext: str, model_dir: Path, where: Bounds):
    """
    Files with the stem, ignoring case, tier by tier (`_Tiers`), each tier in `_Tiers.key`
    order. The whole pack is indexed only when no level has a match.
    """
    tiers = _Tiers(model_dir, where)
    key = tiers.key(ext)
    seen = set()
    for prefix, index in tiers.indexes():
        tier = [((*prefix, *folder), name) for folder, name in index.get(stem, [])]
        for folder, name in sorted(tier, key=lambda entry: key(*entry)):
            if (folder, name) not in seen:
                seen.add((folder, name))
                yield tiers.path(folder, name)


# ── Step 4: heuristics ────────────────────────────────────────────────────

# Stem suffixes, compared ignoring case. The single letters mean base color only after a
# `T_` or `TX_` prefix; elsewhere they are atlas variants.
DIFFUSE_SUFFIXES = (
    "_bc",
    "_basecolor",
    "_base_color",
    "_albedo",
    "_albedotransparency",
    "_color",
    "_colour",
    "_diffuse",
    "_diff",
)
PREFIXED_DIFFUSE_SUFFIXES = ("_b", "_c", "_d")
# A stem whose last `_part` is one of these names another kind of map. All but the single
# letters also count anywhere among the parts the reference-prefix guess adds, which catches
# names with a resolution after the map type, such as Poly Haven's `rock_nor_gl_1k`.
NON_DIFFUSE_WORDS = frozenset(
    {
        # Plan C3 step 4.
        "n", "normal", "normals", "nrm", "orm", "aorm", "r", "roughness", "m", "metallic",
        "metallicsmoothness", "mask", "maskmap", "e", "emission", "emissive", "ao", "height",
        "specular", "smmetao",
        # ambientCG, Poly Haven and other common names.
        "normalgl", "normaldx", "nor", "ambientocclusion", "occlusion", "displacement", "disp",
        "metalness", "rough", "arm", "gloss", "glossiness", "spec", "bump", "opacity", "alpha",
    }
)  # fmt: skip
# Longest first, so `_base_color` isn't read as `_color`.
_DIFFUSE_ORDER = tuple(sorted(DIFFUSE_SUFFIXES, key=len, reverse=True))
_PREFIXED_DIFFUSE_ORDER = tuple(
    sorted(DIFFUSE_SUFFIXES + PREFIXED_DIFFUSE_SUFFIXES, key=len, reverse=True)
)
TEXTURE_PREFIXES = ("t_", "tx_")
CORE_PREFIXES = ("t_", "tx_", "sm_")
MATERIAL_PREFIXES = ("mi_", "mat_", "m_")
DEFAULT_MATERIAL_PREFIXES = ("lambert", "phong", "blinn")
DEFAULT_MATERIALS = frozenset({"defaultmaterial", "material", "no name", "skin"})
# The prefix heuristics never drop a `_part` that would leave a shorter core than this.
CORE_FLOOR = 6
_LOD_SUFFIX = re.compile(r"_lod\d+$")


def _strip_prefix(stem: str, prefixes: tuple[str, ...]) -> str:
    for prefix in prefixes:
        if stem.startswith(prefix):
            return stem[len(prefix) :]
    return stem


@functools.lru_cache(maxsize=1 << 16)
def core_stem(stem: str) -> str:
    """A lowercase stem without a leading `T_`, `TX_` or `SM_`."""
    return _strip_prefix(stem.lower(), CORE_PREFIXES)


@functools.lru_cache(maxsize=1 << 16)
def diffuse_name(stem: str) -> str | None:
    """
    What a diffuse map's stem names: lowercase, without its diffuse suffix and a `T_` or
    `TX_` prefix. None when the stem doesn't end in a diffuse suffix.
    """
    low = stem.lower()
    bare = _strip_prefix(low, TEXTURE_PREFIXES)
    for suffix in _PREFIXED_DIFFUSE_ORDER if bare != low else _DIFFUSE_ORDER:
        if bare.endswith(suffix) and len(bare) > len(suffix):
            return bare[: -len(suffix)]
    return None


def is_non_diffuse(stem: str, added: list[str] | tuple[str, ...] = ()) -> bool:
    """
    Whether a stem names a map other than base color: its last `_part` is in
    `NON_DIFFUSE_WORDS`, or one of the `added` parts is a word there of 2 letters or more.
    """
    parts = stem.lower().split("_")
    if len(parts) > 1 and parts[-1] in NON_DIFFUSE_WORDS:
        return True
    return any(len(part) > 1 and part in NON_DIFFUSE_WORDS for part in added)


def material_core(name: str | None) -> str | None:
    """
    A material name as the material-name heuristic compares it: lowercase, without an `MI_`,
    `M_` or `MAT_` prefix and a trailing `.NNN`. None for a default name or one under 3
    characters.
    """
    low = re.sub(r"\.\d{3}$", "", (name or "").strip().lower())
    low = _strip_prefix(low, MATERIAL_PREFIXES)
    if len(low) < 3 or low.startswith(DEFAULT_MATERIAL_PREFIXES) or low in DEFAULT_MATERIALS:
        return None
    return low


def core_tries(core: str):
    """
    `core`, then `core` with its trailing `_parts` dropped one at a time, stopping before a
    drop would leave under `CORE_FLOOR` characters or only digits and `_`.
    """
    if not core:
        return
    parts = core.split("_")
    yield core
    while len(parts) > 1:
        parts = parts[:-1]
        shorter = "_".join(parts)
        if len(shorter) < CORE_FLOOR or not shorter.strip("_0123456789"):
            return
        yield shorter


def _heuristics(
    reference: str | None,
    material_name: str | None,
    single_material: bool,
    source: str,
    model_dir: Path,
    where: Bounds,
    files_cache: dict | None = None,
):
    """
    Files a material's naming points at (C3 step 4), as `(path, via)` in order, over the
    by-name search's tiers:

    1. `material_name`: a diffuse map named after the material (`material_core`).
    2. `ref_prefix`: any texture not named as another kind of map (`is_non_diffuse`) whose
       core stem is the reference's core, without a `_Texture` part, or starts with it and
       `_`, dropping the reference's trailing `_parts` down to the floor until something
       matches (`core_tries`). A recognised diffuse map (`diffuse_name`) wins, then the fewest
       extra `_parts`.
    3. `file_prefix`, for a model with a single material: a diffuse map named after the
       model's core stem without `_LODn`, dropping its `_parts` the same way.

    Ties go to the nearest tier, then `_Tiers.key` order. `files_cache`, kept for one
    request, lists the tiers' files once for every material of the model.
    """
    name = material_core(material_name)
    ref_core = None
    if reference:
        parts = core_stem(_stem(reference)).split("_")
        ref_core = "_".join(p for p in parts if p != "texture") or "_".join(parts)
    file_core = None
    if single_material:
        file_core = _LOD_SUFFIX.sub("", core_stem(Path(source).stem))
    if not (name or ref_core or file_core):
        return

    tiers = _Tiers(model_dir, where)
    key = tiers.key(_ext(_stem_name(reference)) if reference else "")
    files = files_cache.get((model_dir, where)) if files_cache is not None else None
    if files is None:
        files = _tier_files(tiers)
        if files_cache is not None:
            files_cache[(model_dir, where)] = files

    def ranked(matches):
        """`matches` of `(rank, tier, folder, name)` as paths, best first."""
        matches.sort(key=lambda m: (m[0], m[1], *key(m[2], m[3])))
        return [tiers.path(folder, filename) for _, _, folder, filename in matches]

    def named(want: str):
        return ranked([(0, tier, f, n) for tier, _, diffuse, _, f, n in files if diffuse == want])

    if name:
        for path in named(name):
            yield path, "material_name"

    if ref_core:
        for want in core_tries(ref_core):
            matches = []
            for tier, stem, diffuse, core, folder, filename in files:
                if core != want and not core.startswith(want + "_"):
                    continue
                added = core[len(want) + 1 :].split("_") if core != want else []
                # A known diffuse map is never refused, and beats a name that says nothing.
                if diffuse is None and is_non_diffuse(stem, added):
                    continue
                matches.append(((diffuse is None, len(added)), tier, folder, filename))
            if matches:
                for path in ranked(matches):
                    yield path, "ref_prefix"
                break

    if file_core:
        for want in core_tries(file_core):
            paths = named(want)
            if paths:
                for path in paths:
                    yield path, "file_prefix"
                break


def _tier_files(tiers: _Tiers) -> list[tuple]:
    """
    Every texture file of the tiers once, in its nearest tier, as `(tier, lowercase stem,
    diffuse_name, core_stem, folder parts from top, name)`.
    """
    files, seen = [], set()
    for tier, (prefix, index) in enumerate(tiers.indexes()):
        for stem, entries in index.items():
            diffuse, core = diffuse_name(stem), core_stem(stem)
            for folder, filename in entries:
                entry = ((*prefix, *folder), filename)
                if entry not in seen:
                    seen.add(entry)
                    files.append((tier, stem, diffuse, core, *entry))
    return files


# (folder, library root) -> (built at, {stem: [(folder parts, file name)]}), for whole packs
# and for single levels.
_INDEXES: OrderedDict[tuple[Path, Path], tuple[float, dict]] = OrderedDict()
_LEVELS: OrderedDict[tuple[Path, Path], tuple[float, dict]] = OrderedDict()


def clear_index_cache() -> None:
    _INDEXES.clear()
    _LEVELS.clear()


def _index(
    top: Path, library_root: Path, recursive: bool
) -> dict[str, list[tuple[tuple[str, ...], str]]]:
    """
    The texture files below `top` by lowercase stem, as paths relative to it. `recursive`
    walks the whole tree; otherwise only `top`'s own files and its texture subfolders. The
    walk skips `EXCLUDED_DIRS`, symlinked folders and symlinks out of the library, and stops
    after `INDEX_CAP` entries, counting every folder and file it visits, so a capped tree is
    searched only as far as the walk got. Each index is cached for `INDEX_TTL` seconds.
    """
    cache, size = (_INDEXES, INDEX_ROOTS) if recursive else (_LEVELS, INDEX_LEVELS)
    key = (top, library_root)
    now = time.monotonic()
    cached = cache.get(key)
    if cached and now - cached[0] < INDEX_TTL:
        cache.move_to_end(key)
        return cached[1]

    index: dict[str, list[tuple[tuple[str, ...], str]]] = {}
    visited = 0
    for dirpath, dirnames, filenames in os.walk(top):
        at_top = dirpath == str(top)
        dirnames[:] = sorted(
            d
            for d in dirnames
            if d.lower() not in EXCLUDED_DIRS
            and (recursive or not at_top or d.lower() in TEXTURE_DIRS)
        )
        folder = Path(dirpath).relative_to(top).parts
        visited += 1
        for name in sorted(filenames):
            if visited >= INDEX_CAP:
                break
            visited += 1
            if _ext(name) not in EXTENSIONS:
                continue
            full = os.path.join(dirpath, name)
            if os.path.islink(full) and not _inside(Path(os.path.realpath(full)), library_root):
                continue
            index.setdefault(name.rsplit(".", 1)[0].lower(), []).append((folder, name))
        if visited >= INDEX_CAP:
            break

    cache[key] = (now, index)
    cache.move_to_end(key)
    while len(cache) > size:
        cache.popitem(last=False)
    return index


# ── Loading ───────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class Texture:
    """
    A loaded texture. `status` is `ok`, `unreadable` or `too_large`, with `image` set only
    when `ok`. `alpha` is `cutout` when the source's alpha looks like a cutout, which keeps
    it as an RGBA PNG; `ignored` for any other alpha below 255, dropped like `none`'s.
    """

    status: str
    image: object = None
    alpha: str = "none"


def classify_alpha(alpha) -> str:
    """
    `cutout`, `ignored` or `none` for a Pillow `L` alpha channel, from one histogram: a cutout
    has at least 1% of pixels below 128, at least 10% at 128 or above, and under 25% in the
    mid range 16-239. Uniform 0 alpha and smoothness-like gradients are `ignored`.
    """
    hist = alpha.histogram()
    total = sum(hist)
    if not total or hist[255] == total:
        return "none"
    low = sum(hist[:128])
    mid = sum(hist[16:240])
    if low >= 0.01 * total and total - low >= 0.10 * total and mid < 0.25 * total:
        return "cutout"
    return "ignored"


def load_texture(path: Path, cache: dict | None = None) -> Texture:
    """
    The texture at `path` for a preview GLB, never raising. The size is read from the header
    first, and a source over 8192 px on a side is `too_large`. The alpha is classified at
    source size, then the image shrinks to 1024 px. A cutout stays RGBA, written as PNG;
    anything else is re-encoded as JPEG, which trimesh's exporter keeps as `image/jpeg`.
    `cache`, kept for one request, decodes a shared atlas once.
    """
    if cache is not None and path in cache:
        return cache[path]
    texture = _load(path)
    if cache is not None:
        cache[path] = texture
    return texture


def _load(path: Path) -> Texture:
    from PIL import Image

    try:
        with Image.open(path) as img:
            if max(img.size) > MAX_SOURCE_SIDE:
                return Texture("too_large")
            if img.format == "JPEG":
                img.draft("RGB", (MAX_SIDE, MAX_SIDE))
            has_alpha = "A" in img.getbands() or "transparency" in img.info
            img = img.convert("RGBA" if has_alpha else "RGB")
    except Exception:
        return Texture("unreadable")

    alpha = classify_alpha(img.getchannel("A")) if has_alpha else "none"
    return Texture("ok", fit(img, keep_alpha=alpha == "cutout"), alpha)


def fit(img, keep_alpha: bool):
    """
    An RGB or RGBA `img` shrunk in place to 1024 px on its longest side. With `keep_alpha` it
    stays a plain Pillow image, which trimesh's exporter writes as PNG; otherwise it is
    re-encoded as JPEG, which the exporter keeps as `image/jpeg`.
    """
    from PIL import Image

    if max(img.size) > MAX_SIDE:
        img.thumbnail((MAX_SIDE, MAX_SIDE), Image.LANCZOS)
    if keep_alpha:
        return img

    buf = io.BytesIO()
    img.convert("RGB").save(buf, format="JPEG", quality=JPEG_QUALITY)
    buf.seek(0)
    jpeg = Image.open(buf)
    jpeg.load()
    return jpeg


def shrink_image(img):
    """
    An image a trimesh material holds, through `fit` (plan C6), leaving `img` untouched. Any
    alpha below 255 keeps it PNG, since the source material decides how its alpha is used.
    """
    has_alpha = "A" in img.getbands() or "transparency" in img.info
    img = img.convert("RGBA" if has_alpha else "RGB")
    return fit(img, keep_alpha=has_alpha and img.getchannel("A").getextrema()[0] < 255)


# ── Per-model report ──────────────────────────────────────────────────────

MAX_LISTED = 10  # names kept in each list of the report

DIFFUSE_SLOT = 1
BASE_COLOR_SLOT = 12


def diffuse_reference(ai_material: dict) -> str | None:
    """A material's first diffuse texture path, else its first base color path."""
    slots = ai_material.get("TEXTURES") or {}
    for slot in (DIFFUSE_SLOT, BASE_COLOR_SLOT):
        if slots.get(slot):
            return slots[slot][0]
    return None


def _stem_name(reference: str) -> str:
    """A reference's file name."""
    parts = reference_parts(reference)
    return parts[-1] if parts else reference


def _stem(reference: str) -> str:
    name = _stem_name(reference)
    return name.rsplit(".", 1)[0] if "." in name else name


class SceneTextures:
    """
    Base color textures for one FBX/3DS model, and the `preview_textures` report on them
    (plan C7). Decoded images are cached for the model, so a shared atlas is decoded once.
    """

    def __init__(
        self,
        source: str,
        library_root: str | None,
        pack_root: str | None,
        single_material: bool = False,
    ):
        self.source = source
        self.where = bounds(source, library_root, pack_root)
        # Only a model with one material may take a texture named after its file.
        self.single_material = single_material
        self.cache: dict[Path, Texture] = {}
        self.files: dict = {}  # the heuristics' file lists, shared by the model's materials
        self.report = {
            "materials": 0,
            "textured": 0,
            "via": {},
            "missing": [],
            "unreadable": [],
            "too_large": [],
            "no_uv": 0,
            "alpha": {"cutout": 0, "ignored": 0, "none": 0},
            "drawn_nodes": 0,
            "skipped_nodes": 0,
            "no_root": self.where is None,
        }

    def texture(self, ai_material: dict) -> Texture | None:
        """The material's loaded base color texture, or None to keep its color. Never raises."""
        report = self.report
        report["materials"] += 1
        if self.where is None:
            return None
        reference = diffuse_reference(ai_material)
        name = ai_material.get("NAME") or ""
        try:
            found = resolve_texture(
                reference, self.source, self.where, name, self.single_material, self.files
            )
        except OSError:
            found = None
        if found is None:
            self._list("missing", _stem(reference) if reference else f"material:{name}")
            return None
        path, via = found
        texture = load_texture(path, self.cache)
        if texture.status != "ok":
            self._list(texture.status, path.name)
            return None
        report["textured"] += 1
        report["via"][via] = report["via"].get(via, 0) + 1
        report["alpha"][texture.alpha] += 1
        return texture

    def _list(self, key: str, name: str) -> None:
        names = self.report[key]
        if name not in names and len(names) < MAX_LISTED:
            names.append(name)
