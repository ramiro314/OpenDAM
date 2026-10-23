"""A small reader of an FBX file's node tree, binary or ASCII (plan game-export C12).

It reads what `fbx_facts` needs, the `Objects` and `Connections` sections and the file's
version, and skips everything else: geometry is jumped over unread, and an array property is
kept as its element count, never decoded. So a large FBX reads in milliseconds.
"""

from __future__ import annotations

import mmap
import re
import struct
from dataclasses import dataclass, field
from pathlib import Path

BINARY_MAGIC = b"Kaydara FBX Binary  \x00"

# Top-level sections read, and the objects whose children are skipped
_SECTIONS = frozenset({"FBXHeaderExtension", "Objects", "Connections"})
_SKIPPED = frozenset({"Geometry"})


class FbxError(ValueError):
    """The file is no FBX this reader can read."""


@dataclass(frozen=True)
class Array:
    """An array property, as its element count."""

    count: int


@dataclass
class Node:
    name: str
    props: list = field(default_factory=list)
    children: list[Node] = field(default_factory=list)

    def child(self, name: str) -> Node | None:
        return next((c for c in self.children if c.name == name), None)


@dataclass
class Document:
    version: int
    # The top-level sections read, by name
    sections: dict[str, Node]


def read(path: str | Path) -> Document:
    """Reads the FBX at `path`. Raises `FbxError` when it isn't one, or is cut short."""
    try:
        return _read(path)
    except (struct.error, IndexError) as e:
        raise FbxError(f"the FBX is cut short or malformed ({e})") from e


def _read(path: str | Path) -> Document:
    with open(path, "rb") as f:
        head = f.read(len(BINARY_MAGIC))
        f.seek(0)
        if head == BINARY_MAGIC:
            size = f.seek(0, 2)
            if size < 27:
                raise FbxError("the binary FBX header is cut short")
            with mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as data:
                return _read_binary(data)
        return _read_ascii(f.read().decode("utf-8", errors="replace"))


# ── Binary ────────────────────────────────────────────────────────────────
_SCALARS = {"Y": "<h", "C": "<?", "I": "<i", "F": "<f", "D": "<d", "L": "<q"}


def _read_binary(data) -> Document:
    (version,) = struct.unpack_from("<I", data, 23)
    wide = version >= 7500
    head = struct.Struct("<QQQB" if wide else "<IIIB")
    null = head.size - 1

    def node(offset: int, depth: int) -> tuple[Node | None, int]:
        if offset + head.size > len(data):
            raise FbxError("a node record runs past the end of the file")
        end, num_props, _, name_len = head.unpack_from(data, offset)
        if end == 0:
            return None, offset + null + 1
        if end > len(data) or end <= offset:
            raise FbxError("a node record has a bad end offset")
        offset += head.size
        name = bytes(data[offset : offset + name_len]).decode("utf-8", errors="replace")
        offset += name_len
        props = []
        for _ in range(num_props):
            value, offset = _binary_prop(data, offset)
            props.append(value)
        result = Node(name, props)
        descend = name in _SECTIONS if depth == 0 else not (depth == 1 and name in _SKIPPED)
        while descend and offset < end:
            child, offset = node(offset, depth + 1)
            if child is None:
                break
            result.children.append(child)
        return result, end

    sections: dict[str, Node] = {}
    offset = 27
    while offset < len(data):
        top, offset = node(offset, 0)
        if top is None:
            break
        if top.name in _SECTIONS:
            sections[top.name] = top
    return Document(version, sections)


def _binary_prop(data, offset: int):
    kind = chr(data[offset])
    offset += 1
    if kind in _SCALARS:
        fmt = _SCALARS[kind]
        return struct.unpack_from(fmt, data, offset)[0], offset + struct.calcsize(fmt)
    if kind in "fdlib":
        count, _, compressed = struct.unpack_from("<III", data, offset)
        return Array(count), offset + 12 + compressed
    if kind in "SR":
        (length,) = struct.unpack_from("<I", data, offset)
        offset += 4
        if kind == "R":
            return None, offset + length
        text = bytes(data[offset : offset + length]).decode("utf-8", errors="replace")
        return text, offset + length
    raise FbxError(f"unknown property type {kind!r}")


# ── ASCII ─────────────────────────────────────────────────────────────────
_KEY = re.compile(r"\s*([A-Za-z_][\w|]*)\s*:\s?(.*)$")
_TOKEN = re.compile(r'"([^"]*)"|\*(\d+)|([^,\s{}]+)')
_STRINGS = re.compile(r'"[^"]*"')
_INT = re.compile(r"[-+]?\d+$")
_FLOAT = re.compile(r"[-+]?(\d+\.?\d*|\.\d+)([eE][-+]?\d+)?$")


def _ascii_value(match: re.Match):
    text, count, bare = match.groups()
    if text is not None:
        return text.replace("&quot;", '"')
    if count is not None:
        return Array(int(count))
    if _INT.match(bare):
        return int(bare)
    if _FLOAT.match(bare):
        return float(bare)
    return bare


def _read_ascii(text: str) -> Document:
    if "FBXHeaderExtension" not in text[:4096] and not text.lstrip().startswith("; FBX"):
        raise FbxError("not an FBX file")
    root = Node("")
    stack: list[Node] = [root]
    # Lines left to skip: the depth of braces still open in a skipped node
    skipping = 0
    last: Node | None = None
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith(";"):
            continue
        if skipping:
            bare = _STRINGS.sub("", stripped)
            skipping += bare.count("{") - bare.count("}")
            continue
        if stripped == "}":
            if len(stack) > 1:
                stack.pop()
            continue
        match = _KEY.match(stripped)
        if not match:
            # A value list carried onto the next line
            if last is not None:
                last.props.extend(_ascii_value(m) for m in _TOKEN.finditer(stripped))
            continue
        name, rest = match.groups()
        rest = rest.rstrip()
        opens = rest.endswith("{")
        if opens:
            rest = rest[:-1]
        parent = stack[-1]
        depth = len(stack) - 1
        kept = name in _SECTIONS if depth == 0 else True
        node = Node(name, [_ascii_value(m) for m in _TOKEN.finditer(rest)])
        is_array = len(node.props) == 1 and isinstance(node.props[0], Array)
        skip = not kept or (depth == 1 and name in _SKIPPED) or is_array
        if kept:
            parent.children.append(node)
        last = node
        if opens:
            if skip:
                skipping = 1
            else:
                stack.append(node)

    sections = {n.name: n for n in root.children}
    version = 0
    header = sections.get("FBXHeaderExtension")
    found = header.child("FBXVersion") if header else None
    if found and found.props and isinstance(found.props[0], int):
        version = found.props[0]
    else:
        stated = re.search(r";\s*FBX\s+(\d+)\.(\d+)\.(\d+)", text[:256])
        if stated:
            version = int(stated[1]) * 1000 + int(stated[2]) * 100 + int(stated[3])
    return Document(version, sections)
