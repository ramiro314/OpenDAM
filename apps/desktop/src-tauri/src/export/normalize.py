"""OpenDAM's model normalization (plan game-export C9). The app embeds this script and runs it
with headless Blender, once per model:

    blender -b --factory-startup -P normalize.py -- <job.json> <result.json>

It rewrites the job's FBX to metres, Y up and facing +Z, with its pivot at the bottom centre,
collision meshes dropped and LODs kept, then re-imports what it wrote and reports it. The
result is `{"ok": true, ...}` or `{"ok": false, "error": "..."}`, and `export::blender` checks
it. Positions are reported in Unity's axes: Blender's (x, y, z) is Unity's (-x, z, -y), since
the FBX is written Y up and -Z forward and Unity mirrors X on import.
"""

import json
import math
import sys
import traceback

import bpy
import numpy as np
from io_scene_fbx import parse_fbx
from io_scene_fbx.import_fbx import elem_find_first, elem_name_ensure_class
from mathutils import Matrix, Vector

# `export::blender::NORMALIZE_VERSION`, which a Rust test keeps equal. Bump both when the
# script's output changes.
VERSION = "opendam-normalize/1"

# Each origin's default facing as a turn about Unity's Y, in degrees, positive clockwise seen
# from above (D13): an Unreal model faces +X, turned to +Z.
ORIGIN_YAW = {"unity": 0, "unreal": -90, "generic": 0}

# Object transform properties: a take that keys one on a root object would override the
# normalization, which sits on that object.
OBJECT_TRANSFORMS = {
    "location",
    "rotation_euler",
    "rotation_quaternion",
    "rotation_axis_angle",
    "scale",
    "delta_location",
    "delta_rotation_euler",
    "delta_rotation_quaternion",
    "delta_scale",
}

# Fixed export settings. Unity's import with Scale Factor 1, Convert Units on and Bake Axis
# Conversion off then shows the model in metres, Y up and facing +Z (contract D21). Takes are
# written one NLA strip each, so they keep their source names, not `<Armature>|<Take>`.
EXPORT = {
    "use_selection": False,
    "object_types": {"ARMATURE", "EMPTY", "MESH"},
    "global_scale": 1.0,
    "apply_unit_scale": True,
    "apply_scale_options": "FBX_SCALE_UNITS",
    "axis_forward": "-Z",
    "axis_up": "Y",
    "bake_space_transform": False,
    "use_mesh_modifiers": True,
    "mesh_smooth_type": "FACE",
    "use_tspace": False,
    "use_custom_props": False,
    "add_leaf_bones": False,
    "primary_bone_axis": "Y",
    "secondary_bone_axis": "X",
    "use_armature_deform_only": False,
    "armature_nodetype": "NULL",
    "bake_anim": True,
    "bake_anim_use_all_bones": True,
    "bake_anim_use_nla_strips": True,
    "bake_anim_use_all_actions": False,
    "bake_anim_force_startend_keying": True,
    "bake_anim_step": 1.0,
    "bake_anim_simplify_factor": 1.0,
    # Textures are exported on their own, so the FBX names them without the machine's paths
    "path_mode": "STRIP",
    "embed_textures": False,
    "batch_mode": "OFF",
}


class Refused(Exception):
    """A job the script won't do, with why."""


def is_collision(name: str, patterns: dict) -> bool:
    """`export::collision::PATTERNS`, as the preview worker's `is_collision` reads it."""
    lower = name.lower()
    return any(lower.startswith(p.lower()) for p in patterns["prefixes"]) or any(
        lower.endswith(s.lower()) for s in patterns["suffixes"]
    )


def load(path: str) -> None:
    """Imports the FBX at `path` into an empty scene, with its units and axes as it states
    them. The empty scene keeps `--factory-startup`'s cube and its `Material` out."""
    bpy.ops.wm.read_homefile(use_empty=True)
    try:
        bpy.ops.import_scene.fbx(filepath=path)
    except RuntimeError as e:
        raise Refused(f"Blender can't import it: {str(e).strip()}") from None


def meshes() -> list:
    return sorted((o for o in bpy.data.objects if o.type == "MESH"), key=lambda o: o.name)


def drop_collision(patterns: dict) -> list[str]:
    """Removes the collision meshes, and the mesh data and materials only they used."""
    dropped = [o for o in meshes() if is_collision(o.name, patterns)]
    names = [o.name for o in dropped]
    for obj in dropped:
        bpy.data.objects.remove(obj)
    for mesh in [m for m in bpy.data.meshes if m.users == 0]:
        bpy.data.meshes.remove(mesh)
    for material in [m for m in bpy.data.materials if m.users == 0]:
        bpy.data.materials.remove(material)
    return names


def slots(objects: list) -> list[str]:
    """The materials of `objects`, each once, in object then slot order."""
    names: list[str] = []
    for obj in objects:
        for slot in obj.material_slots:
            if slot.material is not None and slot.material.name not in names:
                names.append(slot.material.name)
    return names


def add_material(objects: list, name: str) -> str | None:
    """Gives each of `objects` with no material the material `name`, which no other material
    may already hold. Returns `name` when it was added."""
    bare = [o for o in objects if not any(s.material for s in o.material_slots)]
    if not bare:
        return None
    if name in bpy.data.materials:
        raise Refused(f"the material to add, {name}, is already one of its materials")
    material = bpy.data.materials.new(name)
    for obj in bare:
        obj.data.materials.clear()
        obj.data.materials.append(material)
        for polygon in obj.data.polygons:
            polygon.material_index = 0
    return name


def stack_names(path: str) -> set[str]:
    """The FBX's take (AnimationStack) names, read from its node tree."""
    root, _version = parse_fbx.parse(path)
    objects = elem_find_first(root, b"Objects")
    if objects is None:
        return set()
    return {
        elem_name_ensure_class(e, b"AnimStack") for e in objects.elems if e.id == b"AnimationStack"
    }


def animates(action) -> bool:
    return any(
        len(curve.keyframe_points) > 0
        for layer in action.layers
        for strip in layer.strips
        for bag in strip.channelbags
        for curve in bag.fcurves
    )


def takes(path: str) -> list[tuple]:
    """The imported takes that animate an object still in the scene, as (object, take, action).
    Blender's importer names each action `<object>|<take>`, or `<object>|<take>|<layer>`."""
    stacks = stack_names(path)
    objects = sorted(bpy.data.objects, key=lambda o: len(o.name), reverse=True)
    found = []
    for action in bpy.data.actions:
        owner = next((o for o in objects if action.name.startswith(o.name + "|")), None)
        if owner is None or not animates(action):
            continue
        rest = action.name[len(owner.name) + 1 :]
        take = rest if rest in stacks or "|" not in rest else rest.rsplit("|", 1)[0]
        found.append((owner, take, action))
    return found


def take_names(found: list[tuple]) -> list[str]:
    return sorted({take for _owner, take, _action in found})


def to_unity(v) -> list[float]:
    return [clean(-v[0]), clean(v[2]), clean(-v[1])]


def clean(value: float) -> float:
    # Rounded to a micrometre, and never -0.0
    return round(value, 6) + 0.0


def bounds(objects: list, matrix=None) -> tuple[Vector, Vector]:
    """The Blender-axes bounds of `objects`' vertices, each at its bind pose, after `matrix`."""
    matrix = matrix or Matrix.Identity(4)
    low, high = np.full(3, np.inf), np.full(3, -np.inf)
    for obj in objects:
        vertices = obj.data.vertices
        if not vertices:
            continue
        local = np.empty(len(vertices) * 3)
        vertices.foreach_get("co", local)
        world = np.array(matrix @ obj.matrix_world)
        points = local.reshape(-1, 3) @ world[:3, :3].T + world[:3, 3]
        low, high = np.minimum(low, points.min(axis=0)), np.maximum(high, points.max(axis=0))
    if np.isinf(low).any():
        raise Refused("its kept meshes have no vertices")
    return Vector(low.tolist()), Vector(high.tolist())


def unity_bounds(low: Vector, high: Vector) -> dict:
    a, b = to_unity(low), to_unity(high)
    return {"min": [min(x, y) for x, y in zip(a, b)], "max": [max(x, y) for x, y in zip(a, b)]}


def report(path: str, patterns: dict) -> dict:
    """What the FBX at `path` holds, re-imported."""
    load(path)
    kept = [o for o in meshes() if not is_collision(o.name, patterns)]
    armatures = [o for o in bpy.data.objects if o.type == "ARMATURE"]
    return {
        "bounds": unity_bounds(*bounds(kept)) if kept else None,
        "meshes": [o.name for o in kept],
        "slots": slots(kept),
        "takes": take_names(takes(path)),
        "bones": [b.name for a in armatures for b in a.data.bones],
        "hasMesh": bool(kept),
        "hasArmature": bool(armatures),
    }


def normalize(job: dict) -> dict:
    if job["kind"] != "model":
        raise Refused(f"normalizing a {job['kind']} isn't built yet")
    origin = job["originFacing"]
    if origin not in ORIGIN_YAW:
        raise Refused(f"unknown origin {origin}")
    patterns = job["patterns"]

    # Steps 1 and 2: an empty scene, the import, and the collision meshes dropped
    load(job["input"])
    dropped = drop_collision(patterns)
    kept = meshes()
    if not kept:
        raise Refused("it has no mesh to keep")
    source_slots = slots(kept)
    source_takes = takes(job["input"])
    added = add_material(kept, job["addedMaterial"])

    # Step 3: one root transform, the facing turn then the pivot translation, on each root
    # object (the armature, when there is one), never on bones
    yaw = (ORIGIN_YAW[origin] + 90 * job["facingQuarterTurns"]) % 360
    roots = [o for o in bpy.data.objects if o.parent is None]
    for owner, take, action in source_takes:
        if owner in roots and any(
            curve.data_path in OBJECT_TRANSFORMS
            for layer in action.layers
            for strip in layer.strips
            for bag in strip.channelbags
            for curve in bag.fcurves
        ):
            raise Refused(f"its take {take} moves its root object {owner.name}")
    # Unity's clockwise yaw is a clockwise turn about Blender's Z, which is negative
    turn = Matrix.Rotation(math.radians(-yaw), 4, "Z")
    low, high = bounds(kept, turn)
    shift = Vector((-(low.x + high.x) / 2, -(low.y + high.y) / 2, -low.z))
    transform = Matrix.Translation(shift) @ turn
    for obj in roots:
        obj.matrix_world = transform @ obj.matrix_world

    # Step 5: the export, each take as one NLA strip under its source name
    for owner, take, action in source_takes:
        data = owner.animation_data or owner.animation_data_create()
        data.action = None
        track = data.nla_tracks.new()
        track.name = take
        strip = track.strips.new(take, int(action.frame_range[0]), action)
        strip.name = take
    bpy.ops.export_scene.fbx(filepath=job["output"], **EXPORT)

    # Step 6: the written file, re-imported
    written = report(job["output"], patterns)
    return {
        **written,
        "version": VERSION,
        "blenderVersion": ".".join(map(str, bpy.app.version)),
        "droppedMeshes": dropped,
        "addedMaterial": added,
        "transform": {
            "originFacing": origin,
            "facingQuarterTurns": job["facingQuarterTurns"],
            "yaw": yaw,
            "pivot": to_unity(shift),
        },
        "source": {"slots": source_slots, "takes": take_names(source_takes)},
    }


def main() -> None:
    job_path, result_path = sys.argv[sys.argv.index("--") + 1 :][:2]
    try:
        with open(job_path, encoding="utf-8") as f:
            result = {"ok": True, **normalize(json.load(f))}
    except Refused as e:
        result = {"ok": False, "error": str(e)}
    except Exception as e:
        traceback.print_exc()
        result = {"ok": False, "error": f"the script failed: {type(e).__name__}: {e}"}
    with open(result_path, "w", encoding="utf-8") as f:
        json.dump(result, f, indent=2)


main()
