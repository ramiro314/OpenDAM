"""Files with no preview end with a `reason` the app shows, not a bare error (plan
import-material-fixes, After the first gate, item 11): an FBX with no mesh, and an Illustrator
file with no PDF to render. An Illustrator file with one gets its thumbnail.
"""

from __future__ import annotations

from pathlib import Path

import pypdfium2 as pdfium
import pytest
from PIL import Image

import worker

FIXTURES = (
    Path(__file__).resolve().parents[3]
    / "apps/desktop/src-tauri/tests/fixtures/game-export/library/Dungeon Props"
)


def preview(source: Path, tmp_path: Path) -> dict:
    return worker.model_meta(str(source), str(tmp_path / "thumbs"), "a1")


def illustrator_file(path: Path, width: float, height: float) -> Path:
    """An Illustrator file as Illustrator 9 and later write it: a PDF, one page this size."""
    pdf = pdfium.PdfDocument.new()
    pdf.new_page(width, height)
    pdf.save(str(path))
    return path


@pytest.mark.parametrize(
    ("size", "thumb"),
    [
        ((612, 792), (1113, 1440)),
        # A poster-sized page is rendered at the thumbnail's size, not at 3x and shrunk
        ((4500, 2600), (1440, 832)),
        ((100, 50), (300, 150)),
    ],
)
def test_an_illustrator_file_gets_a_thumbnail(tmp_path, monkeypatch, size, thumb):
    source = illustrator_file(tmp_path / "icons.ai", *size)
    scales = []
    render = pdfium.PdfPage.render
    monkeypatch.setattr(
        pdfium.PdfPage,
        "render",
        lambda page, **kw: scales.append(kw["scale"]) or render(page, **kw),
    )

    res = preview(source, tmp_path)

    assert res["ok"], res
    assert "reason" not in res
    assert Path(res["thumbnail"]).name == "a1_thumb.png"
    with Image.open(res["thumbnail"]) as img:
        assert img.size == thumb
    assert scales[0] * max(size) <= 1440.5


def test_an_illustrator_file_with_no_pdf_ends_with_a_reason(tmp_path):
    # Illustrator 8 and older write PostScript
    source = tmp_path / "old.ai"
    source.write_bytes(b"%!PS-Adobe-3.0\n%%Creator: Adobe Illustrator(TM) 8.0\n%%EOF\n")

    res = preview(source, tmp_path)

    assert res["ok"] is False
    assert res["reason"] == res["error"] == "No PDF content to preview"
    assert "thumbnail" not in res


def test_an_animation_with_no_mesh_ends_with_a_reason(tmp_path):
    # Bones and a take, as Kenney's Animated Characters animations are, which Assimp can't load
    res = preview(FIXTURES / "Animations/A_GuardIdle.fbx", tmp_path)

    assert res["ok"] is False
    assert res["reason"] == res["error"] == "Animation, no mesh"
    assert res["preview_error"].startswith("FBX/3DS convert failed")
    assert "preview_model" not in res


def test_an_fbx_with_nothing_in_it_ends_with_no_mesh(tmp_path):
    source = tmp_path / "empty.fbx"
    source.write_text(
        '; FBX 7.4.0 project file\nObjects:  {\n\tModel: 1, "Model::Empty", "Null" {\n\t}\n}\n'
        "Connections:  {\n}\n"
    )

    res = preview(source, tmp_path)

    assert res["ok"] is False
    assert res["reason"] == res["error"] == "No mesh"


def test_an_fbx_6_is_a_bare_error(tmp_path):
    # FBX 6 writes a Model as name and subtype, with its geometry inside it, and Assimp refuses
    # many FBX 6 files, meshes and all
    source = tmp_path / "old.fbx"
    source.write_text(
        "; FBX 6.1.0 project file\nFBXHeaderExtension:  {\n\tFBXVersion: 6100\n}\n"
        'Objects:  {\n\tModel: "Model::Cube", "Mesh" {\n\t\tVertices: 0,0,0,1,0,0,0,1,0\n'
        "\t\tPolygonVertexIndex: 0,1,-3\n\t}\n}\n"
    )

    res = preview(source, tmp_path)

    assert res["ok"] is False
    assert "reason" not in res
    assert res["error"].startswith("FBX/3DS convert failed")


def test_an_fbx_with_a_mesh_that_fails_to_convert_is_a_bare_error(tmp_path, monkeypatch):
    import assimp_py

    def fail(path, flags):
        raise RuntimeError("out of memory")

    monkeypatch.setattr(assimp_py, "import_file", fail)
    res = preview(FIXTURES / "Models/SM_Stool.fbx", tmp_path)

    assert res["ok"] is False
    assert "reason" not in res
    assert res["error"] == "FBX/3DS convert failed: out of memory"


def test_an_unreadable_fbx_is_a_bare_error(tmp_path):
    source = tmp_path / "notes.fbx"
    source.write_bytes(b"not a model")

    res = preview(source, tmp_path)

    assert res["ok"] is False
    assert "reason" not in res
    assert res["error"].startswith("FBX/3DS convert failed")
