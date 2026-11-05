# Gate report 2: before deleting the old libraries

The second gate, [the plan's Work item 15](plan.md#work). Measured on 2026-11-04 and 2026-11-05 at
`feb8c19`, the debug binary and frontend of `main` with items 6 to 14 merged (Unity converter
`opendam-unity-glb/6`, item 7's Unreal export, the estimate's hoist note, the thumbnail camera and
the audit's engine grid), in one new clean throwaway profile, `~/.local/share/opendam-import-gate-2`.
As in [the first gate](gate-report.md), every pack went through the real Import pack dialog with its
default steps and no typed hoist, and the app's own `modelThumbnailer` drew every thumbnail. The
driver is `apps/desktop/scripts/import-trial/`, and its checks run against today's `floors.tsv`.

**Every automated check passes, and the deletion waits for the owner's review of the sheets.**
Nothing here deleted, or touched, the old libraries. No floor changed: the floors under
[Proposed floors](#proposed-floors) are for the owner to accept.

| Run | Result | What's left |
|---|---|---|
| All 11 packs | Checks **pass** | The owner's review of 11 sheets |
| `LostRuins.zip`, Unreal route | Checks **pass**, 102 GLBs | The owner's review of its sheet |
| `Waterways.zip`, Unreal route | Checks **pass**, 111 of 161 textured | The owner's review of its sheet. The memory check was waived by the owner, see below |
| Kenney build | **Pass** | Nothing |

## Item 15's own terms

`gate2_check.py`, new beside the first gate's checks, reads them from the GLBs' JSON, the library
DBs and the stage logs. Run over the first gate's profile (version 5) it fails every one of the
converter, LostRuins and hoist rows below, and over this one it passes all 29 checks.

| Term | Result |
|---|---|
| POLYGON's 162 grid GLBs carry their yellow factor | **Pass.** All 162 `PolygonPrototype_Grid_01` GLBs have `[1, 0.4831, 0, 1]`. `SM_Buildings_WindowGlass_01` has its cyan `[0.0051, 0.6596, 0.8714, 1]`, as item 1 measured |
| Fantasy Adventure's 15 GLBs have no billboard mesh | **Pass.** No GLB in the pack keeps a `*_Billboard` mesh beside another, and all 15 trees are built |
| The 3 branch materials are `MASK` | **Pass.** `FAE_WillowBranch`, `FAE_PalmBranch` and `FAE_PineBranch` are `MASK` on every GLB that uses them |
| `tree_b`'s leaf map is on its cards, and every slot change item 4 listed is there | **Pass.** All 162 primitives of [slot-order.tsv](slot-order.tsv), in its 79 GLBs, have their `after` material, `tree_b`'s 48 cards `tree_b_branch` among them |
| LostRuins has 102 GLBs, and no export-type package lacks a GLB or a warning | **Pass.** 102 GLBs, `SM_column04.glb` among them, and its run has no warning, so no package of an export type was left without a GLB (item 7's `noGlb` warning) or failed (`notExported`) |
| Waterways' dialog shows no hoist warning, and reads 111 of 161 under item 12's audit | **Pass.** Its tree has no warning and the note `hoist Meshes: checked once the export runs, since only the Unreal Editor knows which assets it writes. The run warns if the step moves nothing.` It reads 111 of 161, the expected count. `SM_Cylinder` and `Cylinder1`, the redirectors, have no GLB |
| No pack's build warns about a hoist, Wizard Room's included | **Pass.** No run in any library holds a `noHoistFolder` warning, and none holds any warning at all. Wizard Room's dialog still warns `hoist Art: no folder at Art`, as Piece 2 meant, and its build doesn't |

## All 11 packs

Every run ended `done` with no error and no warning, in 16 minutes of builds, and all 4,182 preview
jobs finished. `gate_check.py packs` passes all 11: each pack's folder, manifest and dialog tree
agree, and no entry was skipped.

`glb_audit.py compare` through `pack-map.tsv`:

- **Matches.** Every pack matches its GLB count, and no GLB is unmatched on either side.
- **Absences.** Only the 193 baseline GLBs under `Medieval_Houses_Vol1_Built-In/`.
- **Lost maps.** Exactly the 22 the plan expects: the base and normal map of the billboard material
  in 11 of Fantasy Adventure's 15 trees (`base: 3 to 2 materials`, `normal: 2 to 1 materials`).
  These make `compare` itself exit 1, as the plan says they would. `gate2_check.py` accepts these
  22 and no other.
- **Geometry.** Exactly the plan's expected list of 17: `SM_Bulidings.glb` (0 to 2,116,110
  vertices, 0 to 176 primitives), `SM_Grass_Foliage.glb` (0 to 3,610, 0 to 1), and the 15 trees,
  each 2 meshes to 1, 3 primitives to 2, with the vertex counts in the plan's table to the vertex.

| Pack | GLBs | Textured | Floor | Run (s) | Files in the folder |
|---|---|---|---|---|---|
| Stylized Village | 181 | 158 | 158 | 196 | 2,106 |
| Ancient Ruins | 184 | 176 | 176 | 59 | 2,147 |
| Spooky Cemetery Crypt | 100 | 99 | 99 | 74 | 1,109 |
| Stylized Elven Valley | 200 | 191 | 191 | 126 | 2,071 |
| Stylized Lost Cave | 208 | 198 | 198 | 74 | 1,763 |
| Stylized Wizard Room | 219 | 201 | 201 | 49 | 1,392 |
| Elven Long Bow | 2 | 1 | 1 | 4 | 11 |
| POLYGON Prototype (Synty) | 950 | 909 | 909 | 136 | 5,587 |
| The Big Castle Kit | 293 | 263 | 257 | 63 | 2,199 |
| Fantasy Adventure Environment | 36 | 30 | 26 | 16 | 809 |
| Medieval Houses Vol 1 | 193 | 193 | 193 | 72 | 1,250 |

**Floors.** Every pack reaches its floor. Two are above it: The Big Castle Kit by 6, as in the first
gate (version 5's empty-slot fix, "Piece 1 result"), and Fantasy Adventure by 4, its 4 untextured
billboards gone (item 2).

**Leftovers.** `gate_plan_check.py` matches every leftover to a row of "Piece 1 result" and finds
no problem. Every pack's leftover count equals the plan's, but two that the plan expects to
differ: Medieval Houses' 2 are gone, as the Outcome table expects, and Fantasy Adventure has 6 of
the table's 10, its 4 untextured billboards dropped, as item 2 expects. `gate_plan_check.py` now
names that difference rather than failing it.

**Thumbnails.** Every GLB that takes a preview has a PNG from the app but the 7 collider-only
models, which end `error` as in the first gate (6 Synty `*_Collision`, `FAE_Tree_Colliders`).
Wizard Room's other 180 GLBs are LOD members, which take no preview.

## LostRuins, Unreal route

The run ended `done` with no error, no warning and no failed model, in 4.7 minutes across 4 editor
sessions, with no cap kill. The editor's peak was 6.1 GB of its 20 GB cap.

| Measure | Result |
|---|---|
| GLBs | 102, above 0 and within the scan's estimate of 381. The first gate's 101 plus `SM_column04` |
| Fully textured | 102 of 102, so no leftovers |
| Dialog | No warning, and no note |
| Thumbnails | All 102 have a PNG from the app |
| Disk | 460 MB |

## Waterways, Unreal route

**The memory check didn't pass, and the owner waived it.** At 09:03 on 2026-11-05 the machine had
22.9 GB available, under the plan's 24 GB, with Chrome and the desktop's other apps open. Asked
through Mission Control before the run, the owner chose to run it below 24 GB. The rest of the
check held: `import.unrealMemoryMax` has no stored setting, so it keeps its 20G default, no
benchmark or Unreal Editor was running, and the disk had 1.1 TB free.

The run ended `done` with no error, no warning and no failed model in 14 minutes, across 6 editor
sessions. None was killed, in the app log or the journals, and the app log records no session
recovered or retried.

| Measure | Result |
|---|---|
| GLBs | 161, above 0 and within the scan's estimate of 655 |
| Fully textured | 111 of 161, item 12's expected count |
| Peak memory, the editor's scope | 11.0 GB of its 20 GB cap (`memory.peak`, 10.9G), 10.8 GB of it held, not cache |
| Peak memory, the app's scope | 7.2 GB of its 10 GB cap, 2.5 GB of it held |
| Lowest `MemAvailable` during the run | 3.2 GB |
| Thumbnails | All 161 have a PNG from the app |
| Disk | 2.4 GB |

The editor's peak is below the first gate's 12.7 GB, but `MemAvailable` fell to 3.2 GB, against
12.1 GB in the first gate, because the run started 1.1 GB short of the floor with more of the
machine in use. It stayed above the trial's 1.5 GB guard, which never stopped the app.

**Leftovers.** 50 GLBs: the first gate's 47, and the 3 item 12 now counts as untextured,
`SM_Butterfly_Plane`, `SM_CavernHull` and `SM_Vine_05`, whose only base is the engine's
`WorldGridMaterial_BaseColor`. Read the same way, the first gate's manifest gives the same 50 GLBs
with the same untextured materials. The first gate traced every other material, and nothing in
Waterways' export changed since (item 8 changed no code).

## Kenney build

| Check | Result |
|---|---|
| Offers | The zip opens on `kenney`, then `fab` and `zip` |
| Ticked | The dialog reads "241 packs of 246 ticked" |
| Runs | 241 of 241 `done`, no error and no warning |
| Packs | 241 of 241 built |
| File counts | Every pack matches its scan tree, on disk and in its manifest: 89,459 files |
| Skipped entries | None, in any pack's tree |
| Time | 44 minutes from the dialog to the end of the previews, 41 of them from first to last build, 33 in the builds themselves |
| Disk | 1.7 GB for the library, its database and thumbnails included |
| Memory | Peak 4.6 GB of the 10 GB cap, 2.5 GB of it held, no cap kill |
| Previews | All 56,761 jobs finished |

Item 11's fix shows here: of the first gate's 36 preview errors, the 10 `.ai` files now have a
thumbnail, and the 26 meshless `.fbx` animations end `error` with the reason "Animation, no mesh",
which the UI shows. No asset ends with a bare error. The same 5 packs start unticked as in the first gate.

## Cap kills

There were none, in any stage. `gate-caps.sh` exits 0:

| Stage | App log | User journal OOM | Kernel OOM | Editor sessions killed | Guard stops | Largest peak, and what it held |
|---|---|---|---|---|---|---|
| All 11 packs | 0 | 0 | 0 | 0 | 0 | The app, 10 GB of 10 GB, 2.4 GB held, the rest file cache the cap reclaimed (item 13) |
| LostRuins | 0 | 0 | 0 | 0 | 0 | The editor, 6.1 GB of 20 GB, 3.9 GB held |
| Kenney | 0 | 0 | 0 | 0 | 0 | The app, 4.6 GB of 10 GB, 2.5 GB held |
| Waterways | 0 | 0 | 0 | 0 | 0 | The editor, 11.0 GB of 20 GB, 10.8 GB held |

The 4 and 6 `SIGKILL on client request` lines in the LostRuins and Waterways windows are
`stop_leftovers` clearing each editor session's scope after it ended, as in the first gate. Blender's
largest conversion peaked at 6.7 GB of its 12 GB cap, 2.1 GB of it held.

## Proposed floors

The measured fully textured counts, for the owner to accept. None is lower than today's floor, and
`floors.tsv` is unchanged.

| Pack | GLBs | Floor today | Proposed |
|---|---|---|---|
| Stylarts/Stylized Village | 181 | 158 | 158 |
| Leartes Studios/Ancient Ruins | 184 | 176 | 176 |
| Stylarts/Spooky Cemetery Crypt | 100 | 99 | 99 |
| Stylarts/Stylized Elven Valley | 200 | 191 | 191 |
| Stylarts/Stylized Lost Cave | 208 | 198 | 198 |
| Stylarts/Stylized Wizard Room | 219 | 201 | 201 |
| Manuel Wieser/Elven Long Bow | 2 | 1 | 1 |
| Synty Studios/POLYGON - Prototype Pack - Art by Synty | 950 | 909 | 909 |
| Triplebrick/The Big Castle Kit | 293 | 257 | 263 |
| Staggart Creations/Fantasy Adventure Environment | 36 | 26 | 30 |
| GameAssetFactory/Medieval Houses Modular Vol 1 | 193 | 193 | 193 |
| Fab/Lost Ruins | 102 | none | 102, its first |
| Fab/Waterways | 161 | none | 111, its first |

Accepting them means adding the two Unreal rows to `floors.tsv` and raising Big Castle and Fantasy
Adventure. `gate2_check.py --proposed <file>` writes this table as a floors file.

## Sheets

13 after sheets, from the same path lists as the first gate's, `sheet-paths/` and
`sheet-paths/unreal/`, built by `gate-sheets.sh` from the app's own thumbnails, in
`~/.local/share/opendam-import-gate-2/report/sheets/`. The before sheets are in
`~/.local/share/opendam-import-before-63efa51/sheets/`, and the first gate's after sheets in
`~/.local/share/opendam-import-gate/report/sheets/`. All 13 are registered as evidence for the
owner's review.

What changed on them since the first gate, for the review:

- **Fantasy Adventure.** `FAE_Birch_A`, `FAE_Pine_A`, `FAE_Tree_A` and `FAE_Willow_A` have no
  crossed cards, and the willow's branches are cut out. They are still pink, because
  `FAE_WillowBranch` uses `FAE_Willow_Branch.png`, which is pink (mean RGB 245, 85, 235). The pack
  also ships a green `FAE_Willow_Branch_Green.png`, which no material in the pack uses.
- **POLYGON.** `SM_Buildings_Column_1x3_01`, `SM_Buildings_Door_01` and
  `SM_Buildings_RoofAngledCornerInner_1x1_01` are yellow again, over their grid.
- **Big Castle.** `tree_b` has its bark on the tree and its leaf map on the cards. `grassPlane` now
  faces the camera (item 10), showing its blue-to-white card, and `ivy_a` keeps its white
  background card, the gap accepted in "Piece 1 result".
- **Waterways.** `SM_RockWall_01` is seen from its front (item 14's quarter turn). `SM_Waterblock_01`
  stays a sliver, `SK_Snake` a straight stick and `SM_Fern_01` blocky, the plan's known gaps.

## Known gaps

- **The owner's review of the 13 sheets** is still to come. The deletion waits for it, and the owner
  closes #261 after it.
- **The floors are proposed, not changed.**
- **Waterways ran below the 24 GB memory floor,** by the owner's choice. It passed with no cap kill.
- **The memory figures are from `gate-caps.sh`,** read from `h/memstat.tsv` every 5 s, so a peak
  between samples shows only in `memory.peak`.

## How to read this again

```sh
TRIAL_UI=1 TRIAL_STAGE=packs apps/desktop/scripts/import-trial/run.sh <new folder>
# then, in the same folder, TRIAL_STAGE=lost-ruins, kenney and waterways, each with
# TRIAL_PACKS=apps/desktop/scripts/import-trial/gate-<stage>.json
python3 -I apps/desktop/scripts/import-trial/gate_check.py <folder> <stage>
python3 -I packages/preview-engine/tools/glb_audit.py manifest <folder>/libs/Fab <folder>/libs/Unity \
  --pack <each pack-map.tsv current pack> --out <manifest>
python3 -I packages/preview-engine/tools/glb_audit.py compare \
  docs/plans/import-material-fixes/baseline-v4.tsv <manifest> \
  --pack-map docs/plans/import-material-fixes/pack-map.tsv --floors docs/plans/import-material-fixes/floors.tsv
python3 -I apps/desktop/scripts/import-trial/gate_plan_check.py <manifest>
python3 -I apps/desktop/scripts/import-trial/gate2_check.py <folder> [--proposed <file>]
apps/desktop/scripts/import-trial/gate-caps.sh <folder>
apps/desktop/scripts/import-trial/gate-sheets.sh <folder> <out folder>
```
