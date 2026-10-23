"""Writes the pack import's fixture originals (docs/plans/pack-import/plan.md, Fixtures and
goldens) beside this script, replacing what is there:

    python3 -I make_fixtures.py

It uses only the Python standard library and writes byte for byte (fixed GUIDs, mtimes, owners
and modes, and a gzip header with no name or time), so a rerun gives the same bytes. Its output is
committed, like `tests/fixtures/game-export/`. The fixtures grow with the plan's tickets.

- `synthetic.unitypackage`: GUID folders with `pathname`, `asset` and `asset.meta` entries (and
  folder entries and a `preview.png`, which extraction ignores), all under `Assets/` and the one
  shared top folder `Pack/`. Below it are three wrapper levels (`Built-In/Built-In_Pack/Pack/`)
  beside a `Readme.txt`, then `Art/` and a nested `Houses.unitypackage`. `Art/` holds
  `Models/SM_Log.fbx` (copied from the game export's fixtures), its `.mat` naming a PNG base color
  by GUID, and a `.prefab`. The nested package repeats the outer `Art/T_Rock.png` pathname. It
  has no unsafe entry, so its golden from `extract_unity.py` is exact.
- `escape.unitypackage`: the shared top folder `Esc/`, plus an escaping `../escape.txt`, an
  absolute pathname, a duplicate `pathname`, an asset whose `pathname` ends in `.meta`, a
  hardlink whose link name points outside the target, and a symlink. Extraction skips each of
  the last six and reports it. It has no golden, since `extract_unity.py` differs there by design.
"""

import gzip
import io
import struct
import tarfile
import zlib
from functools import partial
from pathlib import Path

HERE = Path(__file__).resolve().parent
SM_LOG = HERE.parent / "game-export" / "library" / "Unity Moss" / "Models" / "SM_Log.fbx"

# Every member's mtime: 2023-11-14 22:13:20 UTC.
MTIME = 1_700_000_000

# The wrapper levels below the shared top folder.
WRAP = "Assets/Pack/Built-In/Built-In_Pack/Pack"


def png(rgba: tuple[int, int, int, int], size: int = 4) -> bytes:
    """A `size` x `size` RGBA PNG of one colour, with no metadata chunk."""

    def chunk(kind: bytes, body: bytes) -> bytes:
        crc = zlib.crc32(kind + body) & 0xFFFFFFFF
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", crc)

    rows = b"".join(b"\x00" + bytes(rgba) * size for _ in range(size))
    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(rows, 9))
        + chunk(b"IEND", b"")
    )


def meta(guid: str, importer: str = "DefaultImporter:\n  userData:\n") -> bytes:
    return f"fileFormatVersion: 2\nguid: {guid}\n{importer}".encode()


def folder_meta(guid: str) -> bytes:
    return meta(guid, "folderAsset: yes\nDefaultImporter:\n  userData:\n")


def texture_meta(guid: str) -> bytes:
    return meta(guid, "TextureImporter:\n  serializedVersion: 12\n")


class Package:
    """A `.unitypackage` in the making: a gzipped tar of GUID folders."""

    def __init__(self) -> None:
        self.buf = io.BytesIO()
        self.tar = tarfile.open(fileobj=self.buf, mode="w", format=tarfile.PAX_FORMAT)

    def member(self, name: str, data: bytes = b"", kind: bytes = tarfile.REGTYPE, link: str = ""):
        info = tarfile.TarInfo(name)
        info.type = kind
        info.mtime = MTIME
        info.mode = 0o755 if kind == tarfile.DIRTYPE else 0o644
        info.uid = info.gid = 0
        info.uname = info.gname = ""
        info.linkname = link
        info.size = len(data) if kind == tarfile.REGTYPE else 0
        self.tar.addfile(info, io.BytesIO(data) if kind == tarfile.REGTYPE else None)

    def entry(self, guid: str, pathname: str, asset: bytes | None, asset_meta: bytes | None):
        """One GUID folder. Its asset comes before its `pathname`, as in many real packages, so
        extraction can't place a file as it reads it. A folder entry has no asset."""
        self.member(guid, kind=tarfile.DIRTYPE)
        if asset is not None:
            self.member(f"{guid}/asset", asset)
        if asset_meta is not None:
            self.member(f"{guid}/asset.meta", asset_meta)
        # Unity writes a second line, `00`, after the path.
        self.member(f"{guid}/pathname", f"{pathname}\n00".encode())

    def file(self, guid: str, pathname: str, asset: bytes, asset_meta: bytes | None = None):
        self.entry(guid, pathname, asset, asset_meta or meta(guid))

    def folder(self, guid: str, pathname: str):
        self.entry(guid, pathname, None, folder_meta(guid))

    def finish(self) -> bytes:
        self.tar.close()
        return gzip.compress(self.buf.getvalue(), compresslevel=9, mtime=0)


def guid(prefix: str, n: int) -> str:
    return f"{prefix}{n:0{32 - len(prefix)}x}"


MAT_IMPORTER = "NativeFormatImporter:\n  mainObjectFileID: 2100000\n"


def material(name: str, base_color: str) -> bytes:
    """A text-format URP Lit `.mat` whose `_BaseMap` names a texture by GUID."""
    return f"""%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!21 &2100000
Material:
  serializedVersion: 8
  m_Name: {name}
  m_Shader: {{fileID: 4800000, guid: 933532a4fcc9baf4fa0491de14d08ed7, type: 3}}
  m_SavedProperties:
    serializedVersion: 3
    m_TexEnvs:
    - _BaseMap:
        m_Texture: {{fileID: 2800000, guid: {base_color}, type: 3}}
        m_Scale: {{x: 1, y: 1}}
        m_Offset: {{x: 0, y: 0}}
    m_Colors:
    - _BaseColor: {{r: 1, g: 1, b: 1, a: 1}}
""".encode()


def houses() -> bytes:
    """The nested package. Its pathnames repeat the outer package's, so only the source path's
    chain tells its `T_Rock.png` from the outer one."""
    pkg = Package()
    g = partial(guid, "e1")
    pkg.folder(g(1), "Assets/Pack")
    pkg.file(g(2), f"{WRAP}/Art/T_Rock.png", png((90, 90, 100, 255)), texture_meta(g(2)))
    rock_mat = material("M_Rock", g(2))
    pkg.file(g(3), f"{WRAP}/Art/Materials/M_Rock.mat", rock_mat, meta(g(3), MAT_IMPORTER))
    return pkg.finish()


def synthetic() -> bytes:
    pkg = Package()
    g = partial(guid, "a1")
    fbx, mat, base, rock, prefab, nested = g(10), g(11), g(12), g(13), g(14), g(15)

    for n, path in enumerate(
        [
            "Assets/Pack",
            "Assets/Pack/Built-In",
            "Assets/Pack/Built-In/Built-In_Pack",
            WRAP,
            f"{WRAP}/Art",
            f"{WRAP}/Art/Models",
            f"{WRAP}/Art/Materials",
            f"{WRAP}/Art/Textures",
            f"{WRAP}/Art/Prefabs",
        ],
        start=1,
    ):
        pkg.folder(g(n), path)

    pkg.file(g(16), "Assets/Pack/Readme.txt", b"Synthetic pack for OpenDAM's pack import tests.\n")

    fbx_meta = meta(
        fbx,
        f"""ModelImporter:
  serializedVersion: 22200
  externalObjects:
  - first:
      type: UnityEngine:Material
      assembly: UnityEngine.CoreModule
      name: Bark
    second: {{fileID: 2100000, guid: {mat}, type: 2}}
  materials:
    materialImportMode: 2
""",
    )
    pkg.file(fbx, f"{WRAP}/Art/Models/SM_Log.fbx", SM_LOG.read_bytes(), fbx_meta)
    # Unity's own thumbnail of the asset, which extraction ignores.
    pkg.member(f"{fbx}/preview.png", png((0, 0, 0, 255), 2))

    pkg.file(
        mat,
        f"{WRAP}/Art/Materials/M_LogBark.mat",
        material("M_LogBark", base),
        meta(mat, MAT_IMPORTER),
    )
    pkg.file(base, f"{WRAP}/Art/Textures/T_Log_D.png", png((110, 80, 50, 255)), texture_meta(base))
    pkg.file(rock, f"{WRAP}/Art/T_Rock.png", png((120, 120, 120, 255)), texture_meta(rock))

    log_prefab = f"""%YAML 1.1
%TAG !u! tag:unity3d.com,2011:
--- !u!1001 &100100000
PrefabInstance:
  m_SourcePrefab: {{fileID: 100100000, guid: {fbx}, type: 3}}
  m_Modification:
    m_Modifications:
    - target: {{fileID: -7511558181221131132, guid: {fbx}, type: 3}}
      propertyPath: m_Materials.Array.data[0]
      objectReference: {{fileID: 2100000, guid: {mat}, type: 2}}
""".encode()
    prefab_meta = meta(prefab, "PrefabImporter:\n  userData:\n")
    pkg.file(prefab, f"{WRAP}/Art/Prefabs/SM_Log.prefab", log_prefab, prefab_meta)

    pkg.file(nested, f"{WRAP}/Houses.unitypackage", houses())
    return pkg.finish()


def escape() -> bytes:
    pkg = Package()
    g = partial(guid, "b1")
    pkg.folder(g(1), "Assets/Esc")
    pkg.file(g(2), "Assets/Esc/Keep/a.txt", b"kept\n")
    pkg.file(g(3), "Assets/Esc/b.txt", b"first b\n")
    # Not under Assets/: extract_unity.py counts its `..` as a second top folder.
    pkg.file(g(4), "../escape.txt", b"escaped\n")
    pkg.file(g(5), "/tmp/opendam-pack-import-absolute.txt", b"absolute\n")
    pkg.file(g(6), "Assets/Esc/b.txt", b"second b\n")
    pkg.file(g(7), "Assets/Esc/b.txt.meta", b"an asset posing as b.txt's .meta\n")
    pkg.member(g(8), kind=tarfile.DIRTYPE)
    pkg.member(f"{g(8)}/asset", kind=tarfile.LNKTYPE, link="../../../../opendam-hardlink.txt")
    pkg.member(f"{g(8)}/pathname", b"Assets/Esc/hardlink.txt\n00")
    pkg.member(g(9), kind=tarfile.DIRTYPE)
    pkg.member(f"{g(9)}/asset", kind=tarfile.SYMTYPE, link="/etc/passwd")
    pkg.member(f"{g(9)}/pathname", b"Assets/Esc/symlink.txt\n00")
    return pkg.finish()


def main() -> None:
    (HERE / "synthetic.unitypackage").write_bytes(synthetic())
    (HERE / "escape.unitypackage").write_bytes(escape())


if __name__ == "__main__":
    main()
