"""A stand-in for the Unreal Editor's `unreal` module, enough of it for `src/import/unreal_glb.py`
to run under plain Python in the stub editor of `src/import/unreal_glb_tests.rs`, which CI can run
where the real editor isn't.

It reads `$FAKE_UNREAL/registry.json`, the asset registry's list (`[{"package", "asset",
"class"}]`), and `$FAKE_UNREAL/loads.json`, what `load_asset` gives for an object path
(`{"/Game/X/A.A": {"path", "class"}}`, the path differing for a redirector that is followed).
The project's `Content/` folder is `$FAKE_CONTENT`. An export writes a small file naming the
asset's path, and logs go to stdout as the editor's do.
"""

import json
import os
from pathlib import Path

_DIR = Path(os.environ["FAKE_UNREAL"])


def _read(name: str, default):
    path = _DIR / name
    return json.loads(path.read_text()) if path.exists() else default


def log(message: str) -> None:
    print(f"LogPython: {message}", flush=True)


def log_warning(message: str) -> None:
    print(f"LogPython: Warning: {message}", flush=True)


class _Class:
    def __init__(self, name: str):
        self._name = name

    def get_name(self) -> str:
        return self._name


class Object:
    def __init__(self, path: str, cls: str):
        self._path, self._cls = path, cls

    def get_class(self) -> _Class:
        return _Class(self._cls)

    def get_path_name(self) -> str:
        return self._path


class _Name:
    def __init__(self, name: str):
        self.asset_name = name


class AssetData:
    def __init__(self, package: str, asset: str, cls: str):
        self.package_name = package
        self.package_path = package.rsplit("/", 1)[0]
        self.asset_name = asset
        self.asset_class_path = _Name(cls)

    def get_asset(self) -> Object:
        return Object(f"{self.package_name}.{self.asset_name}", self.asset_class_path.asset_name)


class _Registry:
    def search_all_assets(self, synchronous: bool) -> None:
        pass

    def get_assets_by_path(self, path: str, recursive: bool = False) -> list[AssetData]:
        return [AssetData(a["package"], a["asset"], a["class"]) for a in _read("registry.json", [])]


class AssetRegistryHelpers:
    @staticmethod
    def get_asset_registry() -> _Registry:
        return _Registry()


class Paths:
    @staticmethod
    def project_content_dir() -> str:
        return os.environ["FAKE_CONTENT"]

    @staticmethod
    def convert_relative_path_to_full(path: str) -> str:
        return path


def load_asset(object_path: str):
    found = _read("loads.json", {}).get(object_path)
    return Object(found["path"], found["class"]) if found else None


class GLTFMaterialBakeMode:
    USE_MESH_DATA = "USE_MESH_DATA"


class GLTFTextureImageFormat:
    PNG = "PNG"


class GLTFExportOptions:
    def set_editor_property(self, name: str, value) -> None:
        pass


class GLTFExporter:
    @staticmethod
    def export_to_gltf(asset: Object, path: str, options, selection) -> bool:
        Path(path).write_text(f"glTF {asset.get_path_name()}")
        return True


class ScopedSlowTask:
    def __init__(self, work: int, text: str):
        pass

    def __enter__(self):
        return self

    def __exit__(self, *exc) -> None:
        pass

    def make_dialog(self, can_cancel: bool) -> None:
        pass

    def should_cancel(self) -> bool:
        return False

    def enter_progress_frame(self, work: int, text: str) -> None:
        pass


class SystemLibrary:
    @staticmethod
    def collect_garbage() -> None:
        pass

    @staticmethod
    def quit_editor() -> None:
        pass
