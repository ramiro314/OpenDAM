"""Unity's binary `.mat`, `.prefab` and `.unity` files, rewritten as Unity's text format.

The import's `unity_glb` step (plan pack-import, Unity conversion) reads these files as text,
the way Unity 2017 and later write them, with regular expressions. Unity 4 and 5 projects could
store them in the binary serialization instead, which UnityPy reads. `to_text` writes what it
reads back as that text: one `--- !u!<class ID> &<file ID>` document per object, a reference as
`{fileID: <file ID>, guid: <GUID>, type: <type>}` (the GUID from the file's externals, absent for
an object of the same file), a map as a list of `- <key>: <value>` entries, a flat struct such as
a color as `{r: 1, g: 1, b: 1, a: 1}`, and booleans as 0 and 1.

It writes what a reader of that text needs, not a file Unity would load again.
"""

from __future__ import annotations

import json
from pathlib import Path

import numpy

# Bumped with `unity_glb::UNITY_TEXT_VERSION` whenever the text this writes changes, so the
# GLBs made from it are converted again.
VERSION = "opendam-unity-text/1"

HEADER = "%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n"

# A plain scalar can't start with these, as YAML reads it.
_INDICATORS = set("-?:,[]{}#&*!|>'\"%@`")


def guid_text(guid: bytes) -> str:
    """A GUID as Unity writes it in text: each byte's low nibble, then its high one."""
    return "".join(f"{b & 0xF:x}{b >> 4:x}" for b in guid)


def _scalar(value) -> str:
    if isinstance(value, bool):
        return "1" if value else "0"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        if value.is_integer():
            return str(int(value))
        # A float read from a 32-bit field, written as briefly as Unity writes it
        single = numpy.float32(value)
        return str(single) if float(single) == value else repr(value)
    if isinstance(value, bytes | bytearray):
        return value.hex()
    if value is None:
        return ""
    text = str(value)
    plain = (
        text
        and text[0] not in _INDICATORS
        and text == text.strip()
        and "\n" not in text
        and ": " not in text
        and " #" not in text
    )
    return text if plain else json.dumps(text)


def _is_scalar(value) -> bool:
    return value is None or isinstance(value, bool | int | float | str | bytes | bytearray)


class Writer:
    """Writes one serialized file's objects, resolving its references through `externals`."""

    def __init__(self, externals: list):
        self.externals = externals
        self.lines: list[str] = []

    def reference(self, ref: dict) -> dict:
        file_id, path_id = ref["m_FileID"], ref["m_PathID"]
        if file_id == 0 or file_id > len(self.externals):
            return {"fileID": path_id}
        external = self.externals[file_id - 1]
        out = {"fileID": path_id}
        if getattr(external, "guid", None):
            out["guid"] = guid_text(external.guid)
        if getattr(external, "type", None) is not None:
            out["type"] = external.type
        return out

    def value(self, value):
        """`value` with its references resolved."""
        if isinstance(value, dict):
            if set(value) == {"m_FileID", "m_PathID"}:
                return self.reference(value)
            return {k: self.value(v) for k, v in value.items()}
        if isinstance(value, tuple):
            return tuple(self.value(v) for v in value)
        if isinstance(value, list):
            return [self.value(v) for v in value]
        return value

    def entry(self, indent: int, key: str, value) -> None:
        """`key: value` at `indent`, a struct or list on the lines below it."""
        pad = " " * indent
        if isinstance(value, dict) and _is_flat(value):
            self.lines.append(f"{pad}{key}: {_flow(value)}")
        elif isinstance(value, dict):
            self.lines.append(f"{pad}{key}:")
            for k, v in value.items():
                self.entry(indent + 2, k, v)
        elif isinstance(value, list) and not value:
            self.lines.append(f"{pad}{key}: []")
        elif isinstance(value, list):
            self.lines.append(f"{pad}{key}:")
            for item in value:
                self.item(indent, item)
        elif isinstance(value, tuple):
            self.lines.append(f"{pad}{key}:")
            self.entry(indent + 2, "first", value[0])
            self.entry(indent + 2, "second", value[1])
        else:
            self.lines.append(f"{pad}{key}: {_scalar(value)}".rstrip())

    def item(self, indent: int, item) -> None:
        """One list item at `indent`: `- ` then the item, its later lines two columns in. A map's
        entry, a pair, is written `- <key>: <value>`."""
        pad = " " * indent
        at = len(self.lines)
        if isinstance(item, tuple):
            key, value = item
            if isinstance(key, dict) and set(key) == {"name"}:
                key = key["name"]  # Unity 4 and 5's FastPropertyName
            if _is_scalar(key):
                self.entry(indent + 2, _scalar(key), value)
            else:
                self.entry(indent + 2, "first", key)
                self.entry(indent + 2, "second", value)
        elif isinstance(item, dict) and not _is_flat(item):
            for k, v in item.items():
                self.entry(indent + 2, k, v)
        elif isinstance(item, list) and item:
            self.lines.append(f"{pad}  ")
            for sub in item:
                self.item(indent + 2, sub)
        else:
            text = _flow(item) if isinstance(item, dict) else "[]" if item == [] else _scalar(item)
            self.lines.append(f"{pad}  {text}".rstrip())
        self.lines[at] = f"{pad}- {self.lines[at].lstrip()}".rstrip()

    def document(self, class_id: int, path_id: int, class_name: str, tree: dict) -> None:
        self.lines.append(f"--- !u!{class_id} &{path_id}")
        self.entry(0, class_name, self.value(tree))


def _is_flat(value: dict) -> bool:
    """Whether Unity writes the struct `value` on one line, as a reference, a vector or a color."""
    return len(value) <= 4 and all(_is_scalar(v) for v in value.values())


def _flow(value: dict) -> str:
    return "{" + ", ".join(f"{k}: {_scalar(v)}" for k, v in value.items()) + "}"


def to_text(source: Path) -> str:
    """The binary Unity file at `source` in Unity's text format. Raises when UnityPy can't read
    it, or it holds no object."""
    import UnityPy

    env = UnityPy.load(str(source))
    objects = list(env.objects)
    if not objects:
        raise ValueError("UnityPy found no Unity object in it")
    writers: dict[int, Writer] = {}
    for obj in sorted(objects, key=lambda o: o.path_id):
        file = obj.assets_file
        writer = writers.setdefault(id(file), Writer(list(getattr(file, "externals", []))))
        node = obj._get_typetree_node()
        writer.document(obj.class_id, obj.path_id, node.m_Type, obj.read_typetree(node))
    return HEADER + "".join("\n".join(w.lines) + "\n" for w in writers.values())
