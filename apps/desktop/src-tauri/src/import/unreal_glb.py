"""Exports every static mesh, skeletal mesh and animation of an Unreal project to .glb, with
materials baked to PNG textures. OpenDAM's `export_unreal` step (src/import/unreal_glb.rs) runs it
inside a headless Unreal Editor over a work copy of the project, a port of `ue_gltf_export.py`
with its heuristics kept as they are.

Reads the output folder from the GLTF_OUT environment variable. `/Game/Foo/SM_Rock` is written
to `$GLTF_OUT/Foo/SM_Rock.glb`. A file that already exists is skipped, so a session after a crash,
or after OpenDAM put cached GLBs there, picks up where the last one stopped. Each file is written
under a temporary name and renamed once complete, so a killed session leaves no truncated file.

Exports at most GLTF_BATCH new files per editor session (default 30), because the editor's memory
grows with every export. It writes the number still left to `$GLTF_OUT/.gltf-remaining`, and the
packages it failed on to `$GLTF_OUT/.gltf-failed`, which later sessions skip. OpenDAM starts the
editor again until nothing is left or a session makes no progress. Quits the editor when done.
"""

import os
from pathlib import Path

import unreal

VERSION = "opendam-unreal-glb/1"

OUT = Path(os.environ["GLTF_OUT"])
TYPES = {"StaticMesh", "SkeletalMesh", "AnimSequence"}
BATCH = int(os.environ.get("GLTF_BATCH", "30"))


def enum(enum_type: str, *names: str):
    """The first of `names` that this engine version's enum has."""
    cls = getattr(unreal, enum_type)
    for name in names:
        if hasattr(cls, name):
            return getattr(cls, name)
    raise AttributeError(f"{enum_type} has none of {names}")


def options() -> unreal.GLTFExportOptions:
    o = unreal.GLTFExportOptions()
    # Bake each material input the exporter can't match to a texture, using the mesh's own UVs.
    o.set_editor_property(
        "bake_material_inputs", enum("GLTFMaterialBakeMode", "USE_MESH_DATA", "SIMPLE")
    )
    o.set_editor_property("texture_image_format", enum("GLTFTextureImageFormat", "PNG"))
    # Unreal normal maps are DirectX style (green down); glTF and Unity expect green up.
    o.set_editor_property("adjust_normalmaps", True)
    o.set_editor_property("export_vertex_skin_weights", True)
    o.set_editor_property("export_animation_sequences", True)
    o.set_editor_property("export_morph_targets", True)
    # An animation is exported with its skeleton's preview mesh, so it can be viewed alone.
    o.set_editor_property("export_preview_mesh", True)
    return o


def class_name(data: unreal.AssetData) -> str:
    path = getattr(data, "asset_class_path", None)  # UE 5.1+
    return str(path.asset_name) if path is not None else str(data.asset_class)


def main() -> None:
    registry = unreal.AssetRegistryHelpers.get_asset_registry()
    registry.search_all_assets(True)
    assets = [
        a for a in registry.get_assets_by_path("/Game", recursive=True) if class_name(a) in TYPES
    ]
    assets.sort(key=lambda a: str(a.package_name))
    opts = options()
    failed_list = OUT / ".gltf-failed"
    failed_before = set(failed_list.read_text().split()) if failed_list.exists() else set()
    targets = []
    for data in assets:
        rel = str(data.package_path).removeprefix("/Game").strip("/")
        targets.append((data, OUT / rel / f"{data.asset_name}.glb"))
    todo = [
        (d, t) for d, t in targets if not t.exists() and str(d.package_name) not in failed_before
    ]
    done = failed = 0
    unreal.log(f"[gltf] {len(assets)} assets, {len(todo)} left, exporting up to {BATCH} ({VERSION})")
    with unreal.ScopedSlowTask(min(len(todo), BATCH), "Exporting glTF") as task:
        task.make_dialog(True)
        for data, target in todo[:BATCH]:
            if task.should_cancel():
                break
            task.enter_progress_frame(1, str(target.relative_to(OUT)))
            target.parent.mkdir(parents=True, exist_ok=True)
            partial = target.with_name(f".{target.stem}.partial.glb")
            try:
                unreal.GLTFExporter.export_to_gltf(data.get_asset(), str(partial), opts, set())
            except Exception as e:  # one bad asset must not stop the run
                unreal.log_warning(f"[gltf] failed {data.package_name}: {e}")
            if partial.exists():
                partial.rename(target)
                done += 1
                unreal.log(f"[gltf] exported {data.package_name}")
            else:
                failed += 1
                with failed_list.open("a") as f:
                    f.write(f"{data.package_name}\n")
                unreal.log_warning(f"[gltf] no file written for {data.package_name}")
            unreal.SystemLibrary.collect_garbage()
    remaining = len(todo) - done - failed
    (OUT / ".gltf-remaining").write_text(f"{remaining}\n")
    unreal.log(
        f"[gltf] exported {done}, failed {failed}, remaining {remaining}, "
        f"total on disk {sum(t.exists() for _, t in targets)}/{len(targets)}"
    )
    unreal.SystemLibrary.quit_editor()


main()
