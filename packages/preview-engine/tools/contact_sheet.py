"""
Builds a contact sheet from a library's PNG thumbnails (docs/plans/import-material-fixes/plan.md,
"Visual check"). It reads the library DB read-only and writes only `--out`, which must lie outside
the library.

A pack's path list is written once, in baseline terms: one GLB per line, relative to the baseline
pack, as `glb_audit.py` writes it. The pack map turns it into each side's library paths, so a
before and an after sheet show the same models:

    packages/preview-engine/.venv/bin/python packages/preview-engine/tools/contact_sheet.py \\
      ~/after/libs/Fab --pack Stylarts/StylizedVillage --paths village.txt --side current \\
      --pack-map docs/plans/import-material-fixes/pack-map.tsv --out village-after.png

A path the library doesn't hold stops the sheet. A model with no PNG thumbnail, such as one whose
preview failed, gets a marked tile and a line on stderr. The exit status is 1 when no model has
one, because such a sheet shows nothing to review.
"""

from __future__ import annotations

import argparse
import sqlite3
import sys
from contextlib import closing
from pathlib import Path

from glb_audit import AuditError, PackRow, check_outside, read_pack_map
from PIL import Image, ImageDraw, ImageFont

BACKGROUND = (38, 38, 42)
TILE_BACKGROUND = (64, 64, 70)
TEXT = (235, 235, 235)
MISSING_TEXT = (240, 150, 120)
GAP = 8
LABEL_HEIGHT = 20
TITLE_HEIGHT = 32


def read_paths(path: Path) -> list[str]:
    paths = [line.strip() for line in path.read_text(encoding="utf-8").splitlines()]
    paths = [line for line in paths if line and not line.startswith("#")]
    if not paths:
        raise AuditError(f"{path}: no paths")
    return paths


def pack_row(pack_map: list[PackRow], pack: str) -> PackRow:
    for row in pack_map:
        if row.baseline_pack == pack:
            return row
    raise AuditError(f"no pack map row for baseline pack {pack}")


def library_path(row: PackRow, path: str, side: str) -> str:
    """A list path's `assets.file_path` on this side of the map."""
    if not path.startswith(row.baseline_prefix):
        raise AuditError(f"{path} is outside the baseline prefix {row.baseline_prefix}")
    if side == "baseline":
        return f"{row.baseline_pack}/{path}"
    return f"{row.current_pack}/{row.current_prefix}{path.removeprefix(row.baseline_prefix)}"


def read_thumbnails(library: Path, paths: list[str]) -> dict[str, str | None]:
    """Each path's `thumbnail_path`, relative to the library root. Every path must have a row."""
    db = (library / ".opendam" / "library.sqlite").resolve()
    found = {}
    try:
        with closing(sqlite3.connect(f"{db.as_uri()}?mode=ro", uri=True)) as conn:
            for path in paths:
                row = conn.execute(
                    "SELECT thumbnail_path FROM assets WHERE file_path = ?", (path,)
                ).fetchone()
                if row is not None:
                    found[path] = row[0]
    except sqlite3.Error as e:
        raise AuditError(f"{db}: can't read its assets: {e}") from e
    missing = [path for path in paths if path not in found]
    if missing:
        raise AuditError(f"{library} doesn't hold: {', '.join(missing)}")
    return found


def load_tile(library: Path, thumbnail: str | None, size: int) -> Image.Image | None:
    """The thumbnail on a tile, or None when it isn't a PNG the sheet can read."""
    if thumbnail is None or not thumbnail.lower().endswith(".png"):
        return None
    try:
        with Image.open(library / thumbnail) as image:
            image = image.convert("RGBA")
    except OSError:
        return None
    image.thumbnail((size, size))
    tile = Image.new("RGBA", (size, size), TILE_BACKGROUND)
    tile.alpha_composite(image, ((size - image.width) // 2, (size - image.height) // 2))
    return tile


def fit(draw: ImageDraw.ImageDraw, text: str, font, width: int) -> str:
    """Text cut from the left to fit, because a path's end is what tells its files apart."""
    if draw.textlength(text, font=font) <= width:
        return text
    while text and draw.textlength("..." + text, font=font) > width:
        text = text[1:]
    return "..." + text


def build_sheet(
    title: str,
    tiles: list[tuple[str, Image.Image | None]],
    columns: int,
    size: int,
) -> Image.Image:
    rows = (len(tiles) + columns - 1) // columns
    sheet = Image.new(
        "RGB",
        (
            columns * (size + GAP) + GAP,
            TITLE_HEIGHT + rows * (size + LABEL_HEIGHT + GAP) + GAP,
        ),
        BACKGROUND,
    )
    draw = ImageDraw.Draw(sheet)
    font = ImageFont.load_default(size=12)
    draw.text((GAP, 8), fit(draw, title, font, sheet.width - 2 * GAP), fill=TEXT, font=font)
    for index, (label, tile) in enumerate(tiles):
        x = GAP + (index % columns) * (size + GAP)
        y = TITLE_HEIGHT + (index // columns) * (size + LABEL_HEIGHT + GAP)
        if tile is None:
            draw.rectangle((x, y, x + size - 1, y + size - 1), fill=TILE_BACKGROUND)
            draw.text((x + 8, y + 8), "no PNG thumbnail", fill=MISSING_TEXT, font=font)
        else:
            sheet.paste(tile.convert("RGB"), (x, y))
        draw.text((x, y + size + 3), fit(draw, label, font, size), fill=TEXT, font=font)
    return sheet


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Build a contact sheet of a library's thumbnails.")
    parser.add_argument("library", type=Path, help="the library root folder")
    parser.add_argument("--pack", required=True, help="the map's baseline pack")
    parser.add_argument("--paths", required=True, type=Path, help="one baseline path per line")
    parser.add_argument("--side", required=True, choices=["baseline", "current"])
    parser.add_argument("--pack-map", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--columns", type=int, default=4)
    parser.add_argument("--tile", type=int, default=256, help="tile side in pixels")
    args = parser.parse_args(argv)

    try:
        check_outside(args.out, [args.library])
        row = pack_row(read_pack_map(args.pack_map), args.pack)
        listed = read_paths(args.paths)
        paths = [library_path(row, path, args.side) for path in listed]
        thumbnails = read_thumbnails(args.library, paths)
    except (AuditError, OSError) as e:
        print(f"contact_sheet: {e}", file=sys.stderr)
        return 2

    tiles = []
    for path, shown in zip(paths, listed, strict=True):
        tile = load_tile(args.library, thumbnails[path], args.tile)
        if tile is None:
            print(f"no PNG thumbnail: {path}", file=sys.stderr)
        tiles.append((Path(shown).stem, tile))
    title = f"{row.baseline_pack} ({args.side}), {args.library}"
    build_sheet(title, tiles, args.columns, args.tile).save(args.out)
    return 0 if any(tile is not None for _, tile in tiles) else 1


if __name__ == "__main__":
    sys.exit(main())
