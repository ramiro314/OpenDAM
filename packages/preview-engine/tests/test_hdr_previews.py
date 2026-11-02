"""HDR previews: the worker tone maps `.exr`, and `.hdr` is decoded in Rust (`preview.rs`)."""

from __future__ import annotations

from pathlib import Path

import numpy as np
import openexr_numpy
from PIL import Image

import worker


def test_exr_gets_a_tone_mapped_thumbnail(tmp_path: Path):
    source = tmp_path / "sky.EXR"
    linear = np.full((4, 8, 3), 1.0, dtype=np.float32)
    # The extension decides the writer, so it is written first and renamed.
    openexr_numpy.imwrite(str(tmp_path / "sky.exr"), linear)
    (tmp_path / "sky.exr").rename(source)

    res = worker.model_meta(str(source), str(tmp_path / "out"), name="a1")

    assert res["ok"], res
    with Image.open(res["thumbnail"]) as thumb:
        assert Path(res["thumbnail"]).name == "a1_thumb.jpg"
        assert thumb.size == (8, 4)
        # Reinhard and gamma 2.2 take 1.0 to 186.
        assert abs(thumb.convert("RGB").getpixel((4, 2))[0] - 186) <= 2


def test_a_broken_exr_reports_its_failure(tmp_path: Path):
    source = tmp_path / "sky.exr"
    source.write_bytes(b"not an exr")

    res = worker.model_meta(str(source), str(tmp_path / "out"), name="a1")

    assert not res["ok"]
    assert res["error"].startswith("EXR preview failed:")


def test_the_worker_has_no_hdr_branch(tmp_path: Path):
    source = tmp_path / "sky.hdr"
    source.write_bytes(b"#?RADIANCE\n")

    res = worker.model_meta(str(source), str(tmp_path / "out"), name="a1")

    assert res == {
        "ok": False,
        "error": "No preview available for this format",
        "metadata": res["metadata"],
        "preview_version": worker.PREVIEW_VERSION,
    }
