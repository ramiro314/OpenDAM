"""`unity_binary_to_text` (plan pack-import, Unity conversion and Fixtures: binary Unity files).

No binary Unity file can be committed: a real one is Asset Store content. So the request's run on
a real file reads the sample `OPENDAM_UNITY_BINARY_SAMPLE` names (a binary `.mat`, `.prefab` or
`.unity` from a cached Unity 4 or 5 package), and skips with a message without it, in CI too.
The text writer itself is tested here on the trees UnityPy reads from such files.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

import unity_text
import worker

ENGINE = Path(__file__).resolve().parent.parent
SAMPLE = "OPENDAM_UNITY_BINARY_SAMPLE"


def ask(request: dict) -> dict:
    """`request`'s reply from a worker process, over stdio as the app asks."""
    run = subprocess.run(
        [sys.executable, "worker.py"],
        cwd=ENGINE,
        input=json.dumps(request) + "\n",
        capture_output=True,
        text=True,
        timeout=120,
    )
    return json.loads(run.stdout)


def test_a_binary_sample_becomes_text_and_keeps_its_bytes(tmp_path):
    sample = os.environ.get(SAMPLE)
    if not sample:
        pytest.skip(
            f"{SAMPLE} isn't set: name a binary .mat, .prefab or .unity from a cached Unity 4 or 5 "
            "package to run this test"
        )
    source = Path(sample)
    before = hashlib.sha256(source.read_bytes()).hexdigest()
    assert not source.read_bytes().startswith(b"%YAML"), f"{source} is already text"
    output = tmp_path / "text" / "0.txt"

    reply = ask(
        {
            "id": "t1",
            "method": "unity_binary_to_text",
            "params": {"source": str(source), "output": str(output)},
        }
    )

    assert reply == {"id": "t1", "result": {"ok": True, "output": str(output)}}
    assert hashlib.sha256(source.read_bytes()).hexdigest() == before
    text = output.read_text()
    assert text.startswith(unity_text.HEADER)
    docs = re.findall(r"^--- !u!(\d+) &(-?\d+)\n(\w+):$", text, re.MULTILINE)
    assert docs, text[:2000]
    if source.suffix.lower() == ".mat":
        assert ("21", "2100000", "Material") in docs, docs
        assert re.search(r"^\s+m_Name: .+$", text, re.MULTILINE), text[:2000]
        assert "    m_TexEnvs:\n" in text, text[:2000]


def external(guid: str, type_: int) -> SimpleNamespace:
    """A file's external, with the GUID Unity writes as `guid` in text."""
    raw = bytes(int(guid[i + 1] + guid[i], 16) for i in range(0, 32, 2))
    return SimpleNamespace(guid=raw, type=type_, path="")


TEXTURE = "9016de7c0db7965479435bae356efd30"
SHADER = "0000000000000000f000000000000000"


def written(class_id: int, path_id: int, name: str, tree: dict, externals=()) -> str:
    writer = unity_text.Writer(list(externals))
    writer.document(class_id, path_id, name, writer.value(tree))
    return "\n".join(writer.lines) + "\n"


def test_a_unity_4_material_reads_as_the_text_unity_2017_writes():
    tex_env = {
        "m_Texture": {"m_FileID": 2, "m_PathID": 2800000},
        "m_Scale": {"x": 1.0, "y": 1.0},
        "m_Offset": {"x": 0.0, "y": 0.0},
    }
    tree = {
        "m_Name": "Elven Long Bow",
        "m_Shader": {"m_FileID": 1, "m_PathID": 4},
        "m_ShaderKeywords": ["_ALPHATEST_ON"],
        "m_SavedProperties": {
            "m_TexEnvs": [
                ({"name": "_MainTex"}, tex_env),
                ({"name": "_Cube"}, {**tex_env, "m_Texture": {"m_FileID": 0, "m_PathID": 0}}),
            ],
            "m_Floats": [({"name": "_Shininess"}, 0.2485356479883194)],
            "m_Colors": [({"name": "_Color"}, {"r": 1.0, "g": 0.5, "b": 0.25, "a": 1.0})],
        },
    }

    text = written(21, 2100000, "Material", tree, [external(SHADER, 0), external(TEXTURE, 3)])

    assert text == (
        "--- !u!21 &2100000\n"
        "Material:\n"
        "  m_Name: Elven Long Bow\n"
        f"  m_Shader: {{fileID: 4, guid: {SHADER}, type: 0}}\n"
        "  m_ShaderKeywords:\n"
        "  - _ALPHATEST_ON\n"
        "  m_SavedProperties:\n"
        "    m_TexEnvs:\n"
        "    - _MainTex:\n"
        f"        m_Texture: {{fileID: 2800000, guid: {TEXTURE}, type: 3}}\n"
        "        m_Scale: {x: 1, y: 1}\n"
        "        m_Offset: {x: 0, y: 0}\n"
        "    - _Cube:\n"
        "        m_Texture: {fileID: 0}\n"
        "        m_Scale: {x: 1, y: 1}\n"
        "        m_Offset: {x: 0, y: 0}\n"
        "    m_Floats:\n"
        "    - _Shininess: 0.24853565\n"
        "    m_Colors:\n"
        "    - _Color: {r: 1, g: 0.5, b: 0.25, a: 1}\n"
    )


def test_a_prefab_s_renderers_name_their_mesh_and_materials_as_unity_glb_reads_them():
    model, material = "28fda70e65a723849a0959e3cebc669d", "5b75db5cface3e14da6de01973fa2afc"
    externals = [external(model, 3), external(material, 2)]
    game_object = {
        "m_Component": [(4, {"m_FileID": 0, "m_PathID": 400012})],
        "m_Name": "Skeleton_footman_sword",
        "m_IsActive": True,
    }
    filter_ = {
        "m_GameObject": {"m_FileID": 0, "m_PathID": 100012},
        "m_Mesh": {"m_FileID": 1, "m_PathID": 4300000},
    }
    renderer = {
        "m_GameObject": {"m_FileID": 0, "m_PathID": 100012},
        "m_Enabled": True,
        "m_Materials": [{"m_FileID": 2, "m_PathID": 2100000}],
    }
    text = "".join(
        written(*doc, externals)
        for doc in [
            (1, 100012, "GameObject", game_object),
            (33, 3300000, "MeshFilter", filter_),
            (23, 2300000, "MeshRenderer", renderer),
        ]
    )

    assert text == (
        "--- !u!1 &100012\n"
        "GameObject:\n"
        "  m_Component:\n"
        "  - 4: {fileID: 400012}\n"
        "  m_Name: Skeleton_footman_sword\n"
        "  m_IsActive: 1\n"
        "--- !u!33 &3300000\n"
        "MeshFilter:\n"
        "  m_GameObject: {fileID: 100012}\n"
        f"  m_Mesh: {{fileID: 4300000, guid: {model}, type: 3}}\n"
        "--- !u!23 &2300000\n"
        "MeshRenderer:\n"
        "  m_GameObject: {fileID: 100012}\n"
        "  m_Enabled: 1\n"
        "  m_Materials:\n"
        f"  - {{fileID: 2100000, guid: {material}, type: 2}}\n"
    )
    # The script's own pattern for a renderer's materials
    mats = re.search(r"m_Materials:\n((?:\s*- \{[^}]*\}\s*\n)+)", text)
    assert re.findall(r"guid: ([0-9a-f]{32})", mats.group(1)) == [material]


def test_strings_that_would_read_as_yaml_syntax_are_quoted_and_nested_lists_indent():
    tree = {
        "plain": "Rock 01",
        "colon": "a: b",
        "dash": "- not a list",
        "empty": "",
        "lines": [[1, 2], []],
        "pair": ("k", "v"),
        "struct": {"a": 1, "b": 2, "c": 3, "d": 4, "e": 5},
        "none": {},
    }

    assert written(114, 1, "MonoBehaviour", tree) == (
        "--- !u!114 &1\n"
        "MonoBehaviour:\n"
        "  plain: Rock 01\n"
        '  colon: "a: b"\n'
        '  dash: "- not a list"\n'
        '  empty: ""\n'
        "  lines:\n"
        "  -\n"
        "    - 1\n"
        "    - 2\n"
        "  - []\n"
        "  pair:\n"
        "    first: k\n"
        "    second: v\n"
        "  struct:\n"
        "    a: 1\n"
        "    b: 2\n"
        "    c: 3\n"
        "    d: 4\n"
        "    e: 5\n"
        "  none: {}\n"
    )


def test_a_file_unitypy_can_t_read_answers_why_and_writes_nothing(tmp_path):
    source = tmp_path / "M_Broken.mat"
    source.write_bytes(b"\x00\x01 not a serialized file")
    output = tmp_path / "text" / "0.txt"

    result = worker.unity_binary_to_text(str(source), str(output))

    assert result["ok"] is False
    assert result["error"].startswith("UnityPy can't read it: "), result
    assert not output.exists()


def test_an_error_without_a_message_is_named_by_its_type(tmp_path, monkeypatch):
    def fail(source):
        raise KeyError()

    monkeypatch.setattr(unity_text, "to_text", fail)
    monkeypatch.setitem(sys.modules, "unity_text", unity_text)

    result = worker.unity_binary_to_text(str(tmp_path / "M.mat"), str(tmp_path / "0.txt"))

    assert result == {"ok": False, "error": "UnityPy can't read it: KeyError"}


def test_engine_info_finds_unitypy_without_importing_it(monkeypatch):
    monkeypatch.delitem(sys.modules, "UnityPy", raising=False)

    assert worker.engine_info()["unitypy"] is True
    assert "UnityPy" not in sys.modules


def test_without_unitypy_the_request_and_engine_info_say_so(tmp_path, monkeypatch):
    # A `None` entry makes `import UnityPy` raise ImportError.
    monkeypatch.setitem(sys.modules, "UnityPy", None)
    monkeypatch.delitem(sys.modules, "unity_text", raising=False)
    source = tmp_path / "M_Log.mat"
    source.write_bytes(b"\x00binary")

    result = worker.unity_binary_to_text(str(source), str(tmp_path / "0.txt"))

    assert result["ok"] is False
    assert result["error"].startswith("UnityPy can't be imported: "), result
    assert worker.engine_info() == {"preview_version": worker.PREVIEW_VERSION, "unitypy": False}
