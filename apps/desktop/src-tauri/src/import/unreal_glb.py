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
editor again until nothing is left or a session makes no progress. Quits the editor when done,
or when anything fails.

What it exports is the asset registry's assets of an export type, as it lists them, and the
packages on disk below `Content/` that the registry misses (LostRuins' `SM_column04`), each loaded
by path and kept when its own class is an export type. A redirector is skipped: its target is a
package of its own. Every session lists the GLB of every asset it exports that hasn't failed in
`$GLTF_OUT/.gltf-expected`, by the `/Game` path OpenDAM reads back from it, and OpenDAM warns on
any it doesn't find.

Version 3 added the packages the registry misses and `.gltf-expected`.
"""

import os
from pathlib import Path

import unreal

VERSION = "opendam-unreal-glb/3"

OUT = Path(os.environ["GLTF_OUT"])
TYPES = {"StaticMesh", "SkeletalMesh", "AnimSequence"}
REDIRECTOR = "ObjectRedirector"
FAILED = OUT / ".gltf-failed"
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


def on_disk() -> list[str]:
    """The `/Game` package of every `.uasset` below the project's `Content/` folder. World
    Partition's one-file-per-actor folders (`__ExternalActors__`, `__ExternalObjects__`) hold no
    asset of their own, and are left out."""
    content = Path(unreal.Paths.convert_relative_path_to_full(unreal.Paths.project_content_dir()))
    packages = []
    for path in content.rglob("*"):
        if path.suffix.lower() != ".uasset" or not path.is_file():
            continue
        rel = path.relative_to(content).with_suffix("")
        if not any(part.startswith("__External") for part in rel.parts):
            packages.append(f"/Game/{rel.as_posix()}")
    return packages


def fail(package: str) -> None:
    with FAILED.open("a") as f:
        f.write(f"{package}\n")


def glb_game_path(target: Path) -> str:
    """The `/Game` path OpenDAM reads back from the GLB `target`, as it names what it exported."""
    return f"/Game/{target.relative_to(OUT).with_suffix('').as_posix()}"


def write_expected(targets, failed: set[str]) -> None:
    """Lists the GLB of every target that hasn't failed, by `glb_game_path`: OpenDAM warns on
    any it doesn't find. A failed target already warns, under its package."""
    lines = (glb_game_path(t) for p, _, t in targets if p not in failed)
    (OUT / ".gltf-expected").write_text("".join(f"{line}\n" for line in lines))


def unlisted(package: str, failed_before: set[str]):
    """A package the registry misses as a target, `(package, object path, glb)`, when its own
    class is an export type, `None` when it isn't or is a redirector. The asset isn't kept: the
    export loads it again, so only a batch's assets are held. One that already has a GLB, or has
    failed, isn't loaded at all: it was of an export type. One that can't be loaded is recorded
    as failed, so it warns."""
    target = OUT / f"{package.removeprefix('/Game/')}.glb"
    object_path = f"{package}.{package.rsplit('/', 1)[-1]}"
    if target.exists() or package in failed_before:
        return package, object_path, target
    try:
        asset = unreal.load_asset(object_path)
    except Exception as e:
        unreal.log_warning(f"[gltf] {package}: {e}")
        asset = None
    if asset is None:
        failed_before.add(package)
        fail(package)
        unreal.log_warning(f"[gltf] could not load {package}, recorded as failed")
        return None
    cls = asset.get_class().get_name()
    # Loading a redirector can give its target, a package of its own.
    if cls == REDIRECTOR or asset.get_path_name() != object_path:
        unreal.log(f"[gltf] skipped {package}, a redirector")
        return None
    if cls not in TYPES:
        return None
    unreal.log(f"[gltf] found {package}, a {cls} the asset registry doesn't list")
    return package, object_path, target


def main() -> None:
    registry = unreal.AssetRegistryHelpers.get_asset_registry()
    registry.search_all_assets(True)
    listed = registry.get_assets_by_path("/Game", recursive=True)
    known = {str(a.package_name) for a in listed}
    # One package per line: a /Game path can hold spaces.
    failed_before = (
        {line for line in FAILED.read_text().splitlines() if line} if FAILED.exists() else set()
    )
    # (package, its registry data or the object path to load it by, the GLB)
    targets = []
    for data in listed:
        if class_name(data) in TYPES:
            rel = str(data.package_path).removeprefix("/Game").strip("/")
            targets.append((str(data.package_name), data, OUT / rel / f"{data.asset_name}.glb"))
    missed = [p for p in on_disk() if p not in known]
    for package in missed:
        if found := unlisted(package, failed_before):
            targets.append(found)
    if missed:
        # What the class checks loaded is let go before the batch starts.
        unreal.SystemLibrary.collect_garbage()
    targets.sort(key=lambda t: t[0])
    write_expected(targets, failed_before)
    opts = options()
    todo = [(p, a, t) for p, a, t in targets if not t.exists() and p not in failed_before]
    done = failed = 0
    unreal.log(
        f"[gltf] {len(targets)} assets, {len(todo)} left, exporting up to {BATCH} ({VERSION})"
    )
    with unreal.ScopedSlowTask(min(len(todo), BATCH), "Exporting glTF") as task:
        task.make_dialog(True)
        for package, asset, target in todo[:BATCH]:
            if task.should_cancel():
                break
            task.enter_progress_frame(1, str(target.relative_to(OUT)))
            target.parent.mkdir(parents=True, exist_ok=True)
            partial = target.with_name(f".{target.stem}.partial.glb")
            ok = False
            try:
                if isinstance(asset, str):
                    asset = unreal.load_asset(asset)
                else:
                    asset = asset.get_asset()
                result = unreal.GLTFExporter.export_to_gltf(asset, str(partial), opts, set())
                # The exporter reports a failure by returning False (with its messages as a
                # tuple's second item where the engine has them), not always by raising.
                ok = bool(result[0] if isinstance(result, tuple) else result)
            except Exception as e:  # one bad asset must not stop the run
                unreal.log_warning(f"[gltf] failed {package}: {e}")
            if not ok:
                # What a failed export left is no GLB to keep.
                partial.unlink(missing_ok=True)
            if ok and partial.exists():
                partial.rename(target)
                done += 1
                unreal.log(f"[gltf] exported {package}")
            else:
                failed += 1
                failed_before.add(package)
                fail(package)
                unreal.log_warning(f"[gltf] no file written for {package}")
            unreal.SystemLibrary.collect_garbage()
    remaining = len(todo) - done - failed
    write_expected(targets, failed_before)
    (OUT / ".gltf-remaining").write_text(f"{remaining}\n")
    unreal.log(
        f"[gltf] exported {done}, failed {failed}, remaining {remaining}, "
        f"total on disk {sum(t.exists() for _, _, t in targets)}/{len(targets)}"
    )


try:
    main()
finally:
    # Whatever went wrong, the editor quits: OpenDAM waits for it to exit.
    unreal.SystemLibrary.quit_editor()
