"""Writes a textured .glb for each named FBX/OBJ of a Unity pack's work copy, from its Unity
materials (plan pack-import, Unity conversion). OpenDAM embeds this script and runs it inside
Blender, one process per build:

    blender -b --factory-startup --python-exit-code 1 -P unity_glb.py -- <job.json> <result.json>

The job names the work copy (`pack`), a scratch folder, the models to convert, each with the
path its GLB goes to, the longest texture side (`maxTexture`) and the `assimp` program. The
result lists each model as converted, with its material slot count, or failed, with why.

A port of `unity_gltf.py` (`~/Documents/Assets`, sha256 f6e1b992...), whose heuristics it keeps
as they are:

- Materials per mesh come from the prefabs (their own renderers, or material overrides on an
  instance of the model), then the scenes (and, for an OBJ whose `.mtl` is missing, a new slot
  per prefab material): a MeshFilter names the FBX (its GUID) and the mesh (its file ID, named
  in the FBX's `.meta`), and its MeshRenderer lists the materials, one per submesh. A mesh no
  prefab uses gets the `.mat` its FBX `.meta` remaps its material to (`externalObjects`), else
  the one named like it, as Unity's own by-name search does.
- FBX files Blender can't import (ASCII, or binary before FBX 7.1) go through a binary copy
  made with `assimp`.
- Last resort, for packs whose Unity files are binary or missing: textures named after the
  material, the mesh or the file, with a role suffix (`Elven Long Bow Diffuse.png`, `bow_n.png`),
  then after shorter prefixes of the file name (`skeleton_Lying` -> `Skeleton_color.tif`).
- From each `.mat`: the base color texture (else its color), the normal map, and an `_ORM`
  texture (occlusion R, roughness G, metallic B, as glTF expects). Smoothness-based maps
  (`_MetallicGlossMap`, `_MaskMap`) aren't converted. Cutout materials get alpha clipping.
- LOD1 and lower (`*_LOD1`...) and collision meshes (`UCX_*`, `*_ConvexHulls`) are dropped,
  so the file shows one clean mesh.
- Textures are downscaled to `maxTexture` px and embedded as JPEG (PNG when a cutout needs the
  base color's alpha), to keep the files small. The pack's original textures are what Unity
  should use; these GLBs are for browsing.

What differs from the script, so its output depends only on the work copy's files, their names
and the order of their paths, which OpenDAM's cache key covers:

- It converts the models the job names and writes each GLB where the job says. OpenDAM decides
  which models get one (one per stem, none where the pack ships the GLB) and caches them.
- Every lookup that keeps the first of several files reads them sorted by path, lowercased,
  the GUID index included, where the script kept whichever `.meta` `rglob` found last.
- Its JPEG copies and `assimp` conversions go to the job's scratch folder, named by a counter,
  not to `~/.cache/unity-gltf` by Python's per-process `hash()`.
- A texture the FBX or OBJ importer loaded for a material the script leaves as imported is found
  again by its file name in the pack, as the script's own name fallback finds textures, and
  dropped when no file of the pack has that name. The importers look for it by path, relative to
  the model (an FBX's `RelativeFilename`, its folder's subtree, an `.mtl`'s `map_Kd`), so where it
  sat relative to the model would decide the GLB, which a hoist changes. Textures embedded in the
  FBX are kept as they are.
- For the same reason an OBJ's material libraries are found by file name too: Blender imports a
  copy of the OBJ, under its own name, whose `mtllib` lines name the pack's first `.mtl` of each
  name, and drop one the pack has no file of that name for.
"""

import json
import os
import re
import subprocess
import sys
import traceback
from pathlib import Path

import bpy

# OpenDAM records it with each GLB and keys its cache by it: `import::unity_glb::UNITY_GLB_VERSION`,
# which a test keeps equal. Bump both when the GLBs this writes change.
VERSION = "opendam-unity-glb/3"

MODEL_EXTENSIONS = {".fbx", ".obj"}
# Property names are matched lowercased, without underscores: `_Base_Map` is `basemap`.
BASE_PROPS = ["basemap", "maintex", "basecolormap", "mainbasemap", "base", "albedo", "albedomap",
              "diffuse", "diffusemap", "basecolor", "basetexture", "maintexture", "albedotexture",
              "texture", "color"]
# Any other texture whose name has one of these, and none of NOT_BASE, also counts as base color.
BASE_HINTS = ["base", "albedo", "diffuse", "maintex", "color"]
NOT_BASE = ["normal", "bump", "mask", "emis", "orm", "rough", "metal", "smooth", "occl", "ao",
            "opacity", "height", "detail", "second"]
NORMAL_PROPS = ["bumpmap", "normal", "normalmap", "normaltex", "normaltexture"]
ORM_PROPS = ["orm", "ormtex", "arm", "ormmap"]
OPACITY_PROPS = ["opacitymask", "opacitymap", "opacity", "alphamask", "alphamap", "alpha"]
COLOR_PROPS = ["basecolor", "color", "tint", "maincolor"]
LOD_RE = re.compile(r"_lod[1-9]\d*$", re.IGNORECASE)
# Collision meshes: Unreal-style `UCX_` and `_ConvexHulls`, and Unity `_Collider`/`_Collision`.
COLLISION_RE = re.compile(r"^ucx_|_convexhulls?$|_collid(er|ers)$|_collision$", re.IGNORECASE)
GUID_RE = re.compile(r"guid: ([0-9a-f]{32})")


def by_path(paths) -> list[Path]:
    """`paths` in the order every lookup reads them: by path, lowercased."""
    return sorted(paths, key=lambda p: str(p).lower())


# ---------------------------------------------------------------------------- Unity files

def index_guids(pack: Path) -> dict[str, Path]:
    """Every asset's GUID, from its `.meta`. Where two share one, the first by path wins."""
    guids = {}
    for meta in by_path(pack.rglob("*.meta")):
        asset = meta.with_suffix("")
        if asset.is_file():
            match = GUID_RE.search(meta.read_text(errors="replace")[:2000])
            if match:
                guids.setdefault(match.group(1), asset)
    return guids


def mesh_names(fbx_meta: Path) -> dict[str, str]:
    """Mesh file ID -> mesh name, from the FBX's `.meta` (both Unity layouts)."""
    text = fbx_meta.read_text(errors="replace")
    names = {}
    # internalIDToNameTable: - first: {43: 4300000} second: SM_Bush_01_LOD0. Meshes (43) and
    # renderers (23, 137), since prefab overrides name a renderer.
    for fid, name in re.findall(
        r"first:\s*\n?\s*(?:43|23|137):\s*(-?\d+)\s*\n\s*second:\s*(.+)", text
    ):
        names[fid] = name.strip()
    # fileIDToRecycleName: 4300000: SM_Bush_01_LOD0
    for fid, name in re.findall(r"^\s+(43\d{5}):\s*(.+)$", text, re.MULTILINE):
        names.setdefault(fid, name.strip())
    return names


def prefab_materials(pack: Path) -> dict[tuple[str, str], list[str]]:
    """(mesh GUID, mesh file ID) -> material GUIDs, from every prefab and scene; first wins."""
    found: dict[tuple[str, str], list[str]] = {}
    # Prefabs first, then scenes, which list their objects' meshes and materials the same way.
    sources = by_path(pack.rglob("*.prefab")) + by_path(pack.rglob("*.unity"))
    for prefab in sources:
        text = prefab.read_text(errors="replace")
        docs = re.split(r"^--- !u!(\d+) &(-?\d+).*$", text, flags=re.MULTILINE)
        meshes, renderers = {}, {}
        for i in range(1, len(docs) - 2, 3):
            cls, body = docs[i], docs[i + 2]
            owner = re.search(r"m_GameObject: \{fileID: (-?\d+)\}", body)
            if not owner:
                continue
            if cls == "33":  # MeshFilter
                m = re.search(r"m_Mesh: \{fileID: (-?\d+),\s*guid: ([0-9a-f]{32})", body)
                if m:
                    meshes[owner.group(1)] = (m.group(2), m.group(1))
            elif cls in ("23", "137"):  # MeshRenderer, SkinnedMeshRenderer
                # Entries may wrap onto a second line, as YAML lines over 80 columns do.
                mats = re.search(r"m_Materials:\n((?:\s*- \{[^}]*\}\s*\n)+)", body)
                if mats:
                    renderers[owner.group(1)] = re.findall(r"guid: ([0-9a-f]{32})", mats.group(1))
                if cls == "137":
                    m = re.search(r"m_Mesh: \{fileID: (-?\d+),\s*guid: ([0-9a-f]{32})", body)
                    if m:
                        meshes[owner.group(1)] = (m.group(2), m.group(1))
        for go, key in meshes.items():
            if go in renderers and key not in found:
                found[key] = renderers[go]
        # Prefab instances of a model (Unity 2018.3+) override a renderer's materials in place:
        # `target: {fileID: <renderer>, guid: <model>}`, `m_Materials.Array.data[i]`.
        overrides: dict[tuple[str, str], dict[int, str]] = {}
        for fid, guid, i, mat in re.findall(
            r"- target: \{fileID: (-?\d+),\s*guid: ([0-9a-f]{32}),\s*type: \d+\}\s*\n"
            r"\s*propertyPath: m_Materials\.Array\.data\[(\d+)\]\s*\n"
            r"(?:\s*value:.*\n)?\s*objectReference: \{fileID: -?\d+,\s*guid: ([0-9a-f]{32})",
            text,
        ):
            overrides.setdefault((guid, fid), {})[int(i)] = mat
        for key, by_index in overrides.items():
            if key not in found:
                found[key] = [by_index.get(i, "") for i in range(max(by_index) + 1)]
    return found


def norm(prop: str) -> str:
    return prop.lower().replace("_", "")


class UnityMaterial:
    def __init__(self, path: Path, guids: dict[str, Path]):
        text = path.read_text(errors="replace")
        # The Material block's own name; helper blocks before it have an empty `m_Name`.
        block = text[text.find("\nMaterial:"):] if "\nMaterial:" in text else text
        name = re.search(r"^\s+m_Name: (.+)$", block, re.MULTILINE)
        self.path = path
        self.name = name.group(1).strip() if name else path.stem
        self.textures: dict[str, Path] = {}
        for prop, guid in re.findall(
            r"- (\w+):\n\s+m_Texture: \{fileID: -?\d+, guid: ([0-9a-f]{32})", text
        ):
            if guid in guids:
                self.textures.setdefault(norm(prop), guids[guid])
        self.colors: dict[str, tuple[float, ...]] = {}
        for prop, r, g, b, a in re.findall(
            r"- (\w+): \{r: ([-\d.e]+), g: ([-\d.e]+), b: ([-\d.e]+), a: ([-\d.e]+)\}", text
        ):
            self.colors.setdefault(norm(prop), tuple(float(v) for v in (r, g, b, a)))
        self.cutout = "_ALPHATEST_ON" in text or "TransparentCutout" in text

    def texture(self, props: list[str]) -> Path | None:
        return next((self.textures[p] for p in props if p in self.textures), None)

    def base(self) -> Path | None:
        return self.texture(BASE_PROPS) or next(
            (t for p, t in self.textures.items()
             if any(h in p for h in BASE_HINTS) and not any(n in p for n in NOT_BASE)), None)

    def color(self) -> tuple[float, ...] | None:
        return next((self.colors[p] for p in COLOR_PROPS if p in self.colors), None)


# ---------------------------------------------------------------------------- Blender side

IMAGES: dict[tuple[Path, bool], bpy.types.Image] = {}


class Scratch:
    """The job's scratch folder, for JPEG copies and `assimp` conversions."""

    def __init__(self, folder: Path):
        self.folder = folder
        self.count = 0

    def path(self, suffix: str) -> Path:
        self.count += 1
        self.folder.mkdir(parents=True, exist_ok=True)
        return self.folder / f"{self.count}{suffix}"


def image(path: Path, max_size: int, non_color: bool, scratch: Scratch, keep_alpha: bool = False):
    """The texture, downscaled to max_size, and stored as JPEG unless its alpha is needed, which
    keeps the .glb files several times smaller. The originals stay untouched in the pack."""
    key = (path, keep_alpha)
    if key not in IMAGES:
        try:
            img = bpy.data.images.load(str(path), check_existing=False)
        except RuntimeError:
            print(f"  cannot load {path.name}")
            return None
        w, h = img.size
        if max(w, h) > max_size:
            scale = max_size / max(w, h)
            img.scale(max(1, round(w * scale)), max(1, round(h * scale)))
        if not keep_alpha and path.suffix.lower() not in (".jpg", ".jpeg"):
            jpeg = scratch.path(".jpg")
            img.file_format = "JPEG"
            img.save(filepath=str(jpeg), quality=90)
            bpy.data.images.remove(img)
            img = bpy.data.images.load(str(jpeg), check_existing=False)
        img.use_fake_user = True  # survives the per-file cleanup
        IMAGES[key] = img
    img = IMAGES[key]
    if non_color:
        img.colorspace_settings.name = "Non-Color"
    return img


def build_material(um: UnityMaterial, max_size: int, scratch: Scratch) -> bpy.types.Material:
    mat = bpy.data.materials.new(um.name)
    mat.use_nodes = True
    nodes, links = mat.node_tree.nodes, mat.node_tree.links
    bsdf = next(n for n in nodes if n.type == "BSDF_PRINCIPLED")
    base = um.base()
    mask = um.texture(OPACITY_PROPS)
    # The base color's alpha is only needed for a cutout with no separate opacity mask.
    if base and (img := image(base, max_size, False, scratch, keep_alpha=um.cutout and not mask)):
        tex = nodes.new("ShaderNodeTexImage")
        tex.image = img
        links.new(tex.outputs["Color"], bsdf.inputs["Base Color"])
        alpha = tex.outputs["Alpha"]
        if mask and (mask_img := image(mask, max_size, True, scratch)):
            mask_tex = nodes.new("ShaderNodeTexImage")
            mask_tex.image = mask_img
            alpha = mask_tex.outputs["Color"]  # a grayscale mask: its red channel
        if um.cutout or mask:
            clip = nodes.new("ShaderNodeMath")
            clip.operation = "ROUND"  # alpha clip at 0.5 for the glTF exporter
            links.new(alpha, clip.inputs[0])
            links.new(clip.outputs[0], bsdf.inputs["Alpha"])
    elif color := um.color():
        bsdf.inputs["Base Color"].default_value = color
    normal = um.texture(NORMAL_PROPS)
    if normal and (img := image(normal, max_size, True, scratch)):
        tex = nodes.new("ShaderNodeTexImage")
        tex.image = img
        nmap = nodes.new("ShaderNodeNormalMap")
        links.new(tex.outputs["Color"], nmap.inputs["Color"])
        links.new(nmap.outputs["Normal"], bsdf.inputs["Normal"])
    orm = um.texture(ORM_PROPS)
    if orm and (img := image(orm, max_size, True, scratch)):
        tex = nodes.new("ShaderNodeTexImage")
        tex.image = img
        sep = nodes.new("ShaderNodeSeparateColor")
        links.new(tex.outputs["Color"], sep.inputs["Color"])
        links.new(sep.outputs["Green"], bsdf.inputs["Roughness"])
        links.new(sep.outputs["Blue"], bsdf.inputs["Metallic"])
    return mat


def binary_fbx(model: Path, assimp: str, scratch: Scratch) -> Path:
    """The model itself, or for an FBX Blender can't read (ASCII, or binary older than 7.1) a
    binary copy made with the `assimp` command line tool, in the scratch folder."""
    with model.open("rb") as f:
        head = f.read(32)
    if head.startswith(b"Kaydara FBX Binary"):
        if int.from_bytes(head[23:27], "little") >= 7100:
            return model
    elif not head.lstrip().startswith(b";"):
        return model
    out = scratch.path(".fbx")
    try:
        subprocess.run([assimp, "export", str(model), str(out), "-f", "fbx"],
                       check=True, capture_output=True)
    except FileNotFoundError:
        raise RuntimeError(f"it is an ASCII or old FBX, and {assimp} isn't installed to convert it")
    except subprocess.CalledProcessError as e:
        said = (e.stderr or e.stdout or b"").decode(errors="replace").strip().splitlines()
        raise RuntimeError(f"assimp couldn't convert it: {said[-1] if said else e}")
    return out


def rebind_imported_textures(images: dict[str, Path], max_size: int, scratch: Scratch) -> None:
    """Points every texture the importer loaded at the pack's file of that name, through
    `image` like the script's own, or drops it when the pack has none. Embedded ones stay."""
    for mat in bpy.data.materials:
        if not mat.use_nodes:
            continue
        for node in mat.node_tree.nodes:
            img = node.image if node.type == "TEX_IMAGE" else None
            if img is None or img.packed_file is not None:
                continue
            raw = (img.filepath_raw or img.filepath or img.name).replace("\\", "/")
            found = images.get(alnum(Path(raw.rsplit("/", 1)[-1]).stem))
            keep_alpha = node.outputs["Alpha"].is_linked
            non_color = img.colorspace_settings.name == "Non-Color"
            node.image = image(found, max_size, non_color, scratch, keep_alpha) if found else None


def clear_scene() -> None:
    for coll in (bpy.data.objects, bpy.data.meshes, bpy.data.materials, bpy.data.armatures,
                 bpy.data.actions, bpy.data.collections):
        bpy.data.batch_remove(list(coll))


def base_name(name: str) -> str:
    return re.sub(r"\.\d{3}$", "", name)


def obj_for_import(model: Path, mtls: dict[str, Path], scratch: Scratch) -> Path:
    """A copy of the OBJ at `model`, under its own name (Blender names an object without an `o`
    line after the file), whose `mtllib` lines name copies of the pack's `.mtl` files found by
    name, beside no other `.mtl`, so where they sit relative to the model doesn't count."""
    folder = scratch.path("-obj")
    (folder / "mtl").mkdir(parents=True)
    copies: dict[Path, str] = {}

    def library(name: str) -> str | None:
        found = mtls.get(alnum(Path(name.replace("\\", "/").rsplit("/", 1)[-1]).stem))
        if found is None:
            return None
        if found not in copies:
            copies[found] = f"mtl/{len(copies) + 1}.mtl"
            (folder / copies[found]).write_bytes(found.read_bytes())
        return copies[found]

    copy = folder / model.name
    with model.open("rb") as source, copy.open("wb") as out:
        for line in source:
            words = line.split(maxsplit=1)
            if not words or words[0] != b"mtllib":
                out.write(line)
                continue
            rest = words[1].strip().decode(errors="replace") if len(words) > 1 else ""
            # One name with spaces, else several names
            names = [rest] if library(rest) else rest.split()
            found = [lib for lib in map(library, names) if lib]
            if found:
                out.write(("mtllib " + " ".join(found) + "\n").encode())
    return copy


def convert(model: Path, target: Path, guids, by_mesh, by_name, images, mtls, max_size: int,
            assimp: str, scratch: Scratch) -> str:
    clear_scene()
    if model.suffix.lower() == ".fbx":
        bpy.ops.import_scene.fbx(
            filepath=str(binary_fbx(model, assimp, scratch)), use_image_search=False
        )
    else:
        bpy.ops.wm.obj_import(filepath=str(obj_for_import(model, mtls, scratch)))
    rebind_imported_textures(images, max_size, scratch)
    meta = Path(str(model) + ".meta")
    fbx_guid = None
    names: dict[str, str] = {}
    if meta.exists():
        m = GUID_RE.search(meta.read_text(errors="replace")[:2000])
        fbx_guid = m.group(1) if m else None
        names = mesh_names(meta)
    # mesh name -> material GUIDs, for this file's meshes that a prefab uses
    prefab_by_name = {name.lower(): by_mesh[(fbx_guid, fid)]
                      for fid, name in names.items() if (fbx_guid, fid) in by_mesh}
    # Every distinct material list a prefab gives one of this file's meshes, by any mesh ID
    # (an OBJ's mesh IDs aren't named in its `.meta`).
    prefab_lists = list({tuple(m): m for (g, _), m in by_mesh.items() if g == fbx_guid}.values())
    remap = external_objects(meta) if meta.exists() else {}
    lod_file = bool(LOD_RE.search(model.stem))
    built: dict[Path, bpy.types.Material] = {}
    matched = total = 0
    meshes = [o for o in bpy.data.objects if o.type == "MESH"]
    for obj in list(bpy.data.objects):
        name = base_name(obj.name)
        if (LOD_RE.search(name) and not lod_file) or COLLISION_RE.search(name):
            bpy.data.objects.remove(obj)
            continue
        if obj.type != "MESH":
            continue
        guids_for_obj = (prefab_by_name.get(name.lower())
                         or prefab_by_name.get(base_name(obj.data.name).lower()))
        if guids_for_obj is None and len(prefab_lists) == 1:
            guids_for_obj = prefab_lists[0]
        if not obj.material_slots and (guids_for_obj or len(meshes) == 1 and prefab_lists):
            # An OBJ without its .mtl has no slots: give it one per prefab material.
            for _ in (guids_for_obj or prefab_lists[0]):
                obj.data.materials.append(None)
            guids_for_obj = guids_for_obj or prefab_lists[0]
        if not obj.material_slots and not guids_for_obj:
            if named_material([name, model.stem], images):
                obj.data.materials.append(None)
        for i, slot in enumerate(obj.material_slots):
            total += 1
            um = None
            if guids_for_obj and i < len(guids_for_obj) and guids_for_obj[i] in guids:
                um = material_for(guids[guids_for_obj[i]], guids)
            elif slot.material is not None:
                slot_name = base_name(slot.material.name)
                path = (guids.get(remap.get(slot_name, ""))
                        or by_name.get(slot_name.lower()))
                um = material_for(path, guids) if path else None
            if um is not None and not um.textures and not um.colors:
                um = None  # a .mat in Unity's binary format: nothing readable in it
            if um is None:
                keys = (([base_name(slot.material.name)] if slot.material else [])
                        + [name, model.stem] + stem_prefixes(model.stem))
                um = named_material(keys, images)
            if um is None:
                continue
            if um.path not in built:
                built[um.path] = build_material(um, max_size, scratch)
            slot.material = built[um.path]
            matched += 1
    if os.environ.get("UNITY_GLTF_DEBUG"):
        for o in bpy.data.objects:
            print("  obj", o.name, o.type, [(s.link, s.material and s.material.name)
                                            for s in o.material_slots])
    target.parent.mkdir(parents=True, exist_ok=True)
    bpy.ops.export_scene.gltf(filepath=str(target), export_format="GLB",
                              export_image_format="AUTO", export_yup=True)
    return f"{matched}/{total} material slots"


def external_objects(meta: Path) -> dict[str, str]:
    """FBX material name -> `.mat` GUID, from the importer's `externalObjects` remap."""
    text = meta.read_text(errors="replace")
    return {name.strip(): guid for name, guid in re.findall(
        r"type: UnityEngine:Material\n\s+assembly: .*\n\s+name: (.+)\n\s+second: "
        r"\{fileID: -?\d+, guid: ([0-9a-f]{32})", text)}


IMAGE_EXTENSIONS = {".png", ".jpg", ".jpeg", ".tga", ".tif", ".tiff", ".psd", ".bmp"}
# Suffixes after the model or material name that mark a texture's role: `Bow Diffuse`, `bow_n`.
NAME_ROLES = {
    "basemap": ["diffuse", "albedo", "basecolor", "basemap", "color", "colour", "diff", "col",
                "alb", "d", "a", "c", "", "tex", "texture"],
    "normalmap": ["normal", "normals", "normalmap", "nrm", "norm", "nm", "n", "bump"],
}
# Words a base color texture's name may carry that its normal map's name swaps out:
# `pirate_knife_color` -> `pirate_knife_normals`.
COLOR_WORDS = ["color", "colour", "diffuse", "albedo", "basecolor", "diff", "col"]


def alnum(text: str) -> str:
    return re.sub(r"[^a-z0-9]", "", text.lower())


class NamedMaterial(UnityMaterial):
    """A material made from textures named after the model or its material, for packs whose
    Unity files can't be read (binary serialization, no `.meta`)."""

    def __init__(self, key: str, textures: dict[str, Path]):
        self.path = Path(f"name:{key}")
        self.name = key
        self.textures = textures
        self.colors = {}
        self.cutout = False


def stem_prefixes(stem: str) -> list[str]:
    """`skeleton_Lying_01` -> [`skeleton_Lying`, `skeleton`]: shorter names a shared texture may
    use. A one-word prefix must have at least 5 letters, so `SM` or `Prop` match nothing."""
    words = [w for w in re.split(r"[_\s-]+", stem) if w]
    out = []
    for n in range(len(words) - 1, 0, -1):
        prefix = "_".join(words[:n])
        if n > 1 or len(prefix) >= 5:
            out.append(prefix)
    return out


def named_material(keys: list[str], images: dict[str, Path]) -> NamedMaterial | None:
    for key in keys:
        k = alnum(key)
        if len(k) < 3:
            continue
        found = {}
        for role, suffixes in NAME_ROLES.items():
            for suffix in suffixes:  # in preference order
                if k + suffix in images:
                    found[role] = images[k + suffix]
                    break
        if "basemap" in found and "normalmap" not in found:
            base = alnum(found["basemap"].stem)
            for word in COLOR_WORDS:
                if word in base:
                    stem = base.replace(word, "", 1)
                    hit = next((images[stem + n] for n in NAME_ROLES["normalmap"]
                                if stem + n in images), None)
                    # Strip what follows the color word too: `body_color_C` -> `body_normals`.
                    stem2 = base[: base.find(word)]
                    hit = hit or next((images[stem2 + n] for n in NAME_ROLES["normalmap"]
                                       if stem2 + n in images), None)
                    if hit:
                        found["normalmap"] = hit
                        break
        if "basemap" in found:
            return NamedMaterial(key, found)
    return None


MATERIALS: dict[Path, UnityMaterial] = {}


def material_for(path: Path, guids) -> UnityMaterial | None:
    if path.suffix.lower() != ".mat":
        return None
    if path not in MATERIALS:
        MATERIALS[path] = UnityMaterial(path, guids)
    return MATERIALS[path]


def main() -> None:
    argv = sys.argv[sys.argv.index("--") + 1:]
    job = json.loads(Path(argv[0]).read_text())
    result_file = Path(argv[1])
    pack = Path(job["pack"]).resolve()
    scratch = Scratch(Path(job["scratch"]))
    max_size = int(job["maxTexture"])
    assimp = job["assimp"]

    guids = index_guids(pack)
    by_mesh = prefab_materials(pack)
    by_name = {}
    for mat in by_path(pack.rglob("*.mat")):
        by_name.setdefault(mat.stem.lower(), mat)
    images: dict[str, Path] = {}
    for img in by_path(pack.rglob("*")):
        if img.suffix.lower() in IMAGE_EXTENSIONS:
            images.setdefault(alnum(img.stem), img)
    mtls: dict[str, Path] = {}
    for mtl in by_path(pack.rglob("*")):
        if mtl.suffix.lower() == ".mtl":
            mtls.setdefault(alnum(mtl.stem), mtl)
    print(f"[unity-glb] pack: {len(job['models'])} models, {len(by_mesh)} prefab meshes, "
          f"{len(by_name)} materials", flush=True)
    # One `[unity-glb] model` line per model, which OpenDAM counts for its progress.
    results = []
    for entry in job["models"]:
        rel = entry["model"]
        try:
            note = convert(pack / rel, Path(entry["output"]), guids, by_mesh, by_name, images,
                           mtls, max_size, assimp, scratch)
            results.append({"model": rel, "ok": True, "note": note})
            print(f"[unity-glb] model {rel}: {note}", flush=True)
        except Exception as e:  # one bad file must not stop the others' report
            traceback.print_exc()
            results.append({"model": rel, "ok": False, "error": str(e) or type(e).__name__})
            print(f"[unity-glb] model {rel} FAILED: {e}", flush=True)
    result_file.write_text(json.dumps({
        "version": VERSION,
        "blenderVersion": bpy.app.version_string,
        "models": results,
    }))


main()
