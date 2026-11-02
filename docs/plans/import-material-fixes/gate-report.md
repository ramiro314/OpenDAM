# Gate report: before deleting the old libraries

The evidence for [the plan's gate](plan.md#before-deleting-the-old-libraries). Measured on
2026-11-02 at `39c679d`, the debug binary and frontend of `main` with tickets 6 to 14 merged, in one
clean throwaway profile, `~/.local/share/opendam-import-gate`. Every pack went through the real
Import pack dialog with its default steps and no typed hoist, and the app's own `modelThumbnailer`
drew every thumbnail. The driver is `apps/desktop/scripts/import-trial/` (`gate.js`, `run.sh` with
`TRIAL_UI`, `gate_check.py`, `gate-sheets.sh`), and `floors.tsv` beside this report holds the floors.

**The deletion stays on hold.** The owner's review rejected models in four packs and Waterways (see
[The owner's review](#the-owners-review)), and LostRuins' missing mesh still needs a decision.
Nothing here deleted, or touched, the old libraries.

| Run | Result | What holds it |
|---|---|---|
| All 11 packs | Checks **pass**, review **failed** | Rejected models in Fantasy Adventure, Wizard Room, POLYGON and Big Castle |
| `LostRuins.zip`, Unreal route | **Hold** | `SM_column04` is missing with no warning, see below |
| `Waterways.zip`, Unreal route | Checks **pass**, review **failed** | `SM_Fern_01` rejected |
| Kenney build | **Pass** | Nothing |

## All 11 packs

Every run ended `done` with no error and no warning, in 17 minutes of builds, and all 4,182 preview
jobs finished. `glb_audit.py compare` through `pack-map.tsv` passed:

- **Matches.** Every pack matches its GLB count, and no GLB is unmatched on either side.
- **Absences.** Only the 193 baseline GLBs under `Medieval_Houses_Vol1_Built-In/`.
- **Lost maps.** None.
- **Geometry.** Only the expected two: `SM_Bulidings.glb` (0 to 2,116,110 vertices, 0 to 176
  primitives) and `SM_Grass_Foliage.glb` (0 to 3,610 vertices, 0 to 1 primitive).

| Pack | GLBs | Textured | Floor | Run (s) | Files in the folder |
|---|---|---|---|---|---|
| Stylized Village | 181 | 158 | 158 | 201 | 2,106 |
| Ancient Ruins | 184 | 176 | 176 | 63 | 2,147 |
| Spooky Cemetery Crypt | 100 | 99 | 99 | 81 | 1,109 |
| Stylized Elven Valley | 200 | 191 | 191 | 137 | 2,071 |
| Stylized Lost Cave | 208 | 198 | 198 | 80 | 1,763 |
| Stylized Wizard Room | 219 | 201 | 201 | 50 | 1,392 |
| Elven Long Bow | 2 | 1 | 1 | 5 | 11 |
| POLYGON Prototype (Synty) | 950 | 909 | 909 | 153 | 5,587 |
| The Big Castle Kit | 293 | 263 | 257 | 65 | 2,199 |
| Fantasy Adventure Environment | 36 | 26 | 26 | 19 | 809 |
| Medieval Houses Vol 1 | 193 | 193 | 193 | 74 | 1,250 |

**Leftovers.** Every leftover is explained: each pack's list, by material and count, is the plan's
Piece 1 table. Medieval Houses' 2 are gone, because only its URP copy lands, so it has none.

**Thumbnails.** All 2,386 GLBs that take a preview have a PNG from the app, except 7 collider-only
models that end `error` (6 Synty, `FAE_Tree_Colliders`). Wizard Room's other 180 GLBs are LOD
members, which take no preview. Four Big Castle thumbnails are fully transparent, down from 24 at
version 4: `tower_c_top_b_deco_a`, `tower_c_hull`, `wood_part_a` and `wooden_arcade_end`, the
plan's `Material.001` and `Material.002` leftovers, which have a cutout and no texture.

**Worth a look on the sheets.** On Big Castle's, `tree_b` shows pink leaf cards and `ivy_a` a white
card behind the plant, so their cutouts look wrong, and `grassPlane` is seen edge on. The audit
can't judge these. `SM_Tree_01` on Village's has a pale patch in its leaves.

**POLYGON Prototype lost a tint, and the audit can't see it.** 162 of its 950 GLBs use
`PolygonPrototype_Grid_01`, including the column, door and roof corner on its sheet, which went from
yellow to white. Its `.mat` holds a yellow `_Base_Color` and a `_Grid` texture. At version 4 the GLB
took the colour and no texture. At version 5 the plan's only-texture rule binds `_Grid` as the
base, and `build_material` applies a colour only when there is no base texture, so the yellow goes.
The audit passes because it counts the map as gained. The floor of 909 depends on that rule, so
the fix is a converter change, and a version bump, for its own ticket.

**Other colour changes on the sheets are the plan's own fixes,** by each GLB's materials before and
after:

- Wizard Room's `SM_Frame`, white to black: `M_BasicFill` is black, as its file says (rule 5).
- Spooky's `SM_Grass_A`, darker: its sRGB colour is now written linear.
- Medieval Houses' `SM_Plane_01`, white to grey: it gained the texture that closes 193 of 193.

**Memory.** No cap kill. The app's scope peaked at 9.8 GB of its 10 GB cap in this stage, which the
scope's figure doesn't split into file cache and the app's own memory, so a larger import could
reach the cap.

## LostRuins, Unreal route

The run passes as the plan words it, and it is still on hold. It ended `done` with no error and no
failed model in 5 minutes, across 4 editor sessions, with no cap kill. The editor's peak was 5.7 GB
of its 20 GB cap. The pack holds 101 GLBs, above 0 and within the scan's estimate of 381. All 101
are textured, so there are no leftovers.

**A mesh is missing, and nothing says so.** The project has 102 `SM_*` meshes and 101 GLBs:
`/Game/The_Lost_Ruins/Meshes/SM_column04` has none. It is a real `StaticMesh` of 77 KB, and the
export's record lists it neither as exported nor as failed, and reads `complete`.

A separate run of the editor over a fresh copy of the project found why:

- `get_assets_by_path` lists 382 assets and 101 static meshes, without it, and
  `EditorAssetLibrary.does_asset_exist` says False.
- `unreal.load_asset` loads it, and `GLTFExporter.export_to_gltf` writes an 84 KB GLB of it.

So `unreal_glb.py`, which exports what the asset registry lists, skips a mesh the registry misses,
and the run can't tell. Why the registry misses this one is unknown. It is the only one it misses:
a census of the 383 packages under `Content/` found the other 382 in the registry, and all 101
assets of an export type by their own class have a GLB. Reading the zip's `Meshes/` folder
rather than trusting the registry would catch it, and that fix belongs to its own ticket. That
editor run, and the one in the Waterways section, ended with exit status 139 after its script had
finished. They used `-nullrhi`, which the app's sessions don't, and the gate's own sessions weren't
affected.

## Waterways, Unreal route

**The memory check.** It passed on the third reading, on the plan's terms, and the run started
right after it:

| Check | Needed | Found at 13:17 |
|---|---|---|
| `import.unrealMemoryMax` | Its default of 20G | Default, no setting stored |
| `MemAvailable` | At least 24 GB | 24.4 GB |
| Benchmarks and other Unreal runs | None | None |
| Disk | At least 20 GB free | 1,169 GB |

The first two readings failed, at 10.9 GB and then 20.8 GB once the owner had closed their apps,
with about 7 GB held by the kernel and what looked like the NVIDIA driver. The owner chose to
reboot. The gate's profile, and the three runs above, are from before that reboot, and this run
reopened the same profile. The check was not waived.

**The run.** It ended `done` with no error, no warning and no failed model in 13 minutes, across 6
editor sessions. All 6 ended without a cap kill, in the app log or the watcher's, and no session had
to be recovered.

| Measure | Result |
|---|---|
| GLBs | 161, above 0 and within the scan's estimate of 655 |
| Fully textured | 114 of 161 |
| Peak memory, the editor's scope | 12.7 GB of its 20 GB cap, in the first session |
| Peak memory, the app's scope | 8.9 GB of its 10 GB cap |
| Lowest `MemAvailable` during the run | 12.1 GB |
| Thumbnails | All 161 GLBs have a PNG from the app |
| Disk | 2.4 GB |

**No mesh is missing.** LostRuins' finding was checked here too. An editor run read the class of
all 657 packages under `Content/`, and every one is in the registry, so none is skipped the way
`SM_column04` is. The 161 of an export type, 146 static meshes, 9 skeletal meshes and 6 animations,
are exactly the 161 the export recorded, with none failed. `SM_Cylinder` and `Cylinder1` have no
GLB and need none: both are `ObjectRedirector` packages to `SM_CylinderFog`, which exported.

**Leftovers.** All 47 GLBs under the full count are explained. Each material was traced through
its parents in the zip's `.uasset` files, and a GLB can use several:

| Reason | Materials, GLBs |
|---|---|
| Water, waterfalls, fog and depth fade, built from shaders | `MI_DepthFade*` 10, `MI_Waterfall_0*` 11, `MI_Water_Animated` 3, `MI_Waterfall_Transperant_02` 2, `MI_Fog_04` 1 |
| Flat colour, whose master's one texture is `T_Noise_M` | `MI_Brown_02` 5, `MI_Green_0*` 4, `MI_Metal_0*` 3, `MI_White` 3, `MI_Pink*` 2, `MI_Blue_01` 1 |
| Flat colour, with no texture through any parent | `MI_Potion_Color_0*` 16, `MM_PotionColors` 1 |
| A plain, empty material | `MI_PlaneGrass_04` on `SM_PlaneGrass`, 1: despite its name it is a `Material`, not an instance, so it has no parent. The editor reports no used texture and one expression, and the GLB takes the default grey |
| The engine's default material, on a slot with none | `WorldGridMaterial` on `SM_Plane_Sun`, 1 |

The `T_Noise_M` materials lose that noise's variation. That it isn't a colour map is read from its
name and from its being the master's only texture, not from the material graph.

**Two things the count hides.** `SM_Butterfly_Plane`, `SM_CavernHull` and `SM_Vine_05` count as
textured only through a baked `WorldGridMaterial_BaseColor`, the engine's default grid, so they
aren't real textures. And the import dialog warned `hoist Meshes: no folder at Meshes`, though the
hoist worked and every mesh sits at the pack's root. The likely cause is the scan's estimate, which
gives `Content/Cylinder1.uasset` a GLB, so its `flatten` keeps `Waterways/`. It would mislead
someone importing this pack.

**Worth a look on the sheet.** `SK_Snake` draws as a thin straight line, `SM_RockWall_01` as a dark
scrap with a vine, and `SM_Waterblock_01` as a thin sliver.

## Kenney build

| Check | Result |
|---|---|
| Offers | The zip opens on `kenney`, then `fab` and `zip` |
| Ticked | The dialog reads "241 packs of 246 ticked" and "Import 241 packs" |
| Runs | 241 of 241 `done`, no error and no warning |
| Packs | 241 of 241 built |
| File counts | Every pack matches its scan tree, on disk and in its manifest: 89,459 files |
| Skipped entries | None, in any pack's tree |
| Time | 47 minutes for the stage, 43 of them from first to last build, 35 of that in the builds themselves |
| Disk | 1.7 GB for the library, its database and thumbnails included |
| Memory | Peak 3.8 GB of the 10 GB cap, no cap kill |
| Previews | All 56,760 jobs finished |

The zip's other files aren't lost, but they aren't in the library either:

- **5 packs start unticked,** 1,721 files by their trees: `Archive/Mini Car Kit`,
  `Archive/Onscreen Controls`, `Archive/Space Kit (Legacy)`, `Early access/Medieval Weapons` and
  `Early access/Racing Kit`.
- **108 files belong to no pack,** because no license file marks their folder: `Goodies/`, `Other/`
  (fonts and miniguides), `Archive/Isometric Renders` and `Pre-rendered Models`, and a few others.
- **36 assets end `error`, and they keep no preview:** 26 `.fbx` animations, which Assimp can't
  load, in the Animated Characters packs, and 10 `.ai` files.

## Sheets

There are 11 before and 11 after sheets, from the same path lists in `sheet-paths/`, and sheets of
12 models each for LostRuins and Waterways from `sheet-paths/unreal/`. They are the app's own
thumbnails, in `~/.local/share/opendam-import-before-63efa51/sheets/` and
`~/.local/share/opendam-import-gate/report/sheets/`.

## The owner's review

The owner reviewed the sheets and rejected the models below as still needing work. Every automated
check passed on them, so this is what the audit can't see. Each cause was traced in the GLB and its
source files, except where the table says it wasn't. None is fixed here: a fix changes the converter.

| Models | What shows | Cause | Fix would need |
|---|---|---|---|
| Fantasy Adventure `FAE_Birch_A`, `FAE_Pine_A`, `FAE_Tree_A`, `FAE_Willow_A` | Solid crossed cards around the tree | Each GLB keeps its `_Billboard` mesh, an opaque far-distance stand-in, beside `_LOD0`. 15 of the pack's GLBs do. It looked the same at version 4, so it is no regression | Dropping a billboard beside a real mesh. That changes geometry, so the plan's expected list changes too |
| `FAE_Willow_A`, also `FAE_PalmBranch` and `FAE_PineBranch` materials | Solid pink branch cards | The cutout is read from `_ALPHATEST_ON`, which 3 of the 6 `FAE_TreeBranch` materials lack, on one shared shader. `FAE_Willow_Branch.png` is 62% transparent, but the GLB drops its alpha | A cutout rule that also reads the shader, after checking that the graph clips |
| POLYGON `SM_Buildings_Door_01`, `..._RoofAngledCornerInner_1x1_01` | White, were yellow | The lost tint above, on 162 GLBs | Applying `_Base_Color` over an only-texture base. Version 5's own change |
| Big Castle `tree_b` | Big pink cards, thin scraps | Slots look swapped. The FBX has `branch.001` on 48 faces, then `Apple1.001` on 3,196, and the prefab lists `bark`, then `tree_b_branch`. So the 48 big cards get bark and the 3,196-face tree gets the leaf map. The likely cause is that Unity's submesh order differs from Blender's slot order. That is inferred, not proved, and how many other GLBs it reaches is unknown | Matching a prefab's list to Unity's submesh order |
| Big Castle `ivy_a`, `grassPlane` | White card behind the ivy, a grey sliver | Each has a mesh with no material, the plan's "slot no Unity file resolves" (`Plane.081` on the ivy). `grassPlane` also faces edge on to the thumbnail's fixed camera | Finding the slot's real material, if any file holds it. The camera is a separate change |
| Wizard Room `SM_Books3_2` | Plain white | Confirmed by GUID: no prefab or scene names this mesh, and it has no `.mtl`, so no Unity file assigns a material. The same holds for `SM_Walls2`, the plan's 12 GLBs with their LODs. `M_Book*.mat` files exist, but linking them is the filename rule this plan rejected | An owner's decision: accept the gap, or allow a name rule for it |
| Waterways `SM_Fern_01` | Blocky leaf ends | **Cause not found.** Its GLB is `MASK`, but the baked base is 96% opaque, as is the bake of `SM_Fern_02`, `_03` and `_04`, 4 of the pack's 9 `MASK` materials. Why Unreal's bake keeps so little of the mask wasn't traced | Reading `MI_Fern_02`'s material graph in the editor |

No other model was marked. The owner's rejection of `SM_Books3_2` also reopens a gap the plan had
accepted, and so may apply to `ivy_a` and `grassPlane`.

## How to read this again

```sh
apps/desktop/scripts/import-trial/run.sh <new folder>  # TRIAL_UI=1 and TRIAL_STAGE=packs
python3 -I apps/desktop/scripts/import-trial/gate_check.py <folder> packs
apps/desktop/scripts/import-trial/gate-sheets.sh <folder> <out folder>
```

The later stages reopen the same folder, with `TRIAL_PACKS` set to their `gate-*.json` and a new
`TRIAL_STAGE`. The real UI is the built frontend served on the app's devUrl port, with its one IPC
call site sent through `gate.js`, which answers the native file picker. So the page runs without
the embedded frontend's CSP, and grid images may not load in it (`docs/agents/running-the-app.md`,
"Scroll benchmark"). Neither touches thumbnails, which are drawn on a canvas from the GLB.
