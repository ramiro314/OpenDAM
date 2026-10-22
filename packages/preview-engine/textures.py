"""
Texture lookup and loading for FBX/3DS previews (plan textured-fbx-previews, C3 and C5).

`resolve_texture` finds the file a material's texture reference names, reading nothing outside
the library root, and `load_texture` turns it into an image for the preview GLB: at most
1024 px, JPEG unless its alpha looks like a cutout.
"""

from __future__ import annotations

import io
import os
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
    reference: str | None, source: str, where: Bounds | None
) -> tuple[Path, str] | None:
    """
    The file a material's texture reference names, as `(path, via)`, or None.

    `via` is the step that found it: `given` (an absolute path), `relative` (the path joined
    to the model's folder, each component ignoring case) or `name` (the file name, ignoring
    case and extension, nearest first). A non-PSD file found by any step beats every PSD; the
    first PSD found is used only when no step finds anything else. An embedded (`*N`)
    reference, or one without a root (`where` is None), finds nothing.
    """
    if not reference or reference.startswith("*") or where is None:
        return None
    held = None
    for found in _candidates(reference, Path(os.path.realpath(source)).parent, where):
        if not _is_psd(found[0]):
            return found
        held = held or found
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


def _by_name(stem: str, ext: str, model_dir: Path, where: Bounds):
    """
    Files with the stem, ignoring case, tier by tier. A tier is a level of the walk up from
    the model's folder: that folder and its texture subfolders, recursively. Then the whole
    pack. Within a tier the reference's own extension comes first, then the others in
    `EXTENSIONS` order, then the nearest folder, then the path ignoring case. Each tier is
    indexed only when the lookup reaches it, so the whole pack is walked only when no level
    has a match, and without a pack nothing is walked beyond the levels.
    """
    levels = [model_dir]
    if where.pack_root:
        while levels[-1] != where.pack_root:
            levels.append(levels[-1].parent)
    else:
        while len(levels) <= NO_PACK_LEVELS and levels[-1] != where.library_root:
            levels.append(levels[-1].parent)
    top = levels[-1]
    model_rel = model_dir.relative_to(top).parts

    def key(entry: tuple[tuple[str, ...], str]):
        folder, name = entry
        e = _ext(name)
        order = 0 if e == ext and ext != "psd" else 1 + EXTENSIONS.index(e)
        common = 0
        while common < min(len(folder), len(model_rel)) and folder[common] == model_rel[common]:
            common += 1
        distance = len(folder) + len(model_rel) - 2 * common
        return order, distance, "/".join((*folder, name)).lower()

    def tiers():
        for level in levels:
            prefix = level.relative_to(top).parts
            entries = _index(level, where.library_root, recursive=False).get(stem, [])
            yield [((*prefix, *folder), name) for folder, name in entries]
        if where.pack_root:
            yield _index(top, where.library_root, recursive=True).get(stem, [])

    seen = set()
    for tier in tiers():
        for folder, name in sorted(tier, key=key):
            if (folder, name) not in seen:
                seen.add((folder, name))
                yield top.joinpath(*folder, name)


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


def _stem(reference: str) -> str:
    parts = reference_parts(reference)
    name = parts[-1] if parts else reference
    return name.rsplit(".", 1)[0] if "." in name else name


class SceneTextures:
    """
    Base color textures for one FBX/3DS model, and the `preview_textures` report on them
    (plan C7). Decoded images are cached for the model, so a shared atlas is decoded once.
    """

    def __init__(self, source: str, library_root: str | None, pack_root: str | None):
        self.source = source
        self.where = bounds(source, library_root, pack_root)
        self.cache: dict[Path, Texture] = {}
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
        if reference is None:
            self._list("missing", f"material:{ai_material.get('NAME') or ''}")
            return None
        try:
            found = resolve_texture(reference, self.source, self.where)
        except OSError:
            found = None
        if found is None:
            self._list("missing", _stem(reference))
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
