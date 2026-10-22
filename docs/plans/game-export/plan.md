# Game export: collections to Dungeon Game bundles

Status: shaped 2026-10-22 in a three-round interview (D1-D34 below). Once this plan merges,
Mission Control slices it into tickets.

## Goal

The Dungeon Game takes its art only as **bundles** that OpenDAM exports and the game's importer
(`make art BUNDLE=<path>`) validates and places. The game side has decided the contract. This
plan builds OpenDAM's half of it. When it is done:

- You pick an export root folder and a root collection (`DungeonGame`) in Settings. Each child
  collection of that root is a **domain** (`Warden`, `Valuables`, `Dungeon`, ...).
- A domain's **Export** view lists every asset with its game name and status, the problems that
  block the export, and the diff against the last export. **Export** writes
  `<export root>/DungeonGame/<Domain>/` with `manifest.json` and exactly the files it lists.
- A **Game** tab in the detail panel shows and lets you correct each asset's `assetName`, its
  rig and facing, its materials (slot, role and texture), and each texture's role, channel layout
  and normal-map convention. Inferred values are marked as inferred, and your edits as yours.
- Every exported model has been rewritten by headless Blender to metres, Y up, facing +Z, with
  its pivot at the bottom centre. Every texture is ready for its URP Lit role. Every manifest is
  valid against the contract's schema, and every conversion is recorded.

## The contract is the source of truth

The contract lives in the private repository `ramiro314/dungeon-game`:

| File | What it is |
|---|---|
| `docs/asset-contract.md` | The prose spec, contract version 1 |
| `docs/asset-contract.schema.json` | JSON Schema 2020-12 for `manifest.json` and (under `$defs/conversionFile`) `conversion.json` |
| `docs/adr/0002-opendam-asset-contract.md` | The contract's decisions D1-D21 and the rejected options |
| `docs/plans/opendam-importer/plan.md` | The game's importer plan, which says what the importer checks |

This plan implements the contract as of dungeon-game commit
`05279adae1cf2151af12a3ef307f27eac695b1ad`, the schema's last change. The schema and spec are
the same at `origin/main` `dfab743` (checked 2026-10-22). OpenDAM never drifts from it. If this
plan or its implementation needs the contract to change, the change is proposed in dungeon-game
first, with a `contractVersion` bump if it breaks either side, and the schema is then re-copied
here (C1). This interview found no contract change to propose.

What the contract fixes, in brief:

- **The bundle:** `<Domain>/manifest.json`, `Models/<assetName>.fbx`,
  `Textures/<assetName>_<Role>.<png|tga|psd|tif>`, `Animations/<assetName>.fbx`, and nothing
  else. One export is the whole domain: what the manifest doesn't list leaves the game.
- **The manifest:** `contractVersion` 1, `domain`, `exportedAt`, `exporter` (`OpenDAM` and its
  version), `packs` (id, name, version, publisher, sourceUrl, licence `{spdx, name, url}`,
  origin), and `assets`. Each asset has kind, assetName, file, sha256, pack, sourcePath and
  conversion. A model adds bounds, rig, clips and materials (`{slot, name, surface, textures}`).
  A texture adds a role. An animation adds avatarFrom and clips.
- **Roles:** `BaseColor` (sRGB, alpha as opacity for cutout), `Normal` (OpenGL, green up),
  `MetallicSmoothness` (metal in R, smoothness in A), `Occlusion` (AO in G), `Emission` (sRGB),
  `Height` (G). The importer never edits pixels.
- **Normalization:** with Scale Factor 1, Convert Units on and Bake Axis Conversion off, Unity
  shows the model at real size in metres, Y up, facing +Z, with `min.y` 0 and the centre on X and
  Z at 0. The importer refuses a size off by more than 1% on any axis, or a pivot off by more
  than 1% of the largest dimension.
- **Names:** no two models or animations share an `assetName`, and no two textures share an
  `assetName` and role. A model and its own textures share an `assetName` on purpose.
- **Conversions:** a step is `{tool, toolVersion, by?, from, settings?}`, with flat scalar
  settings. A `conversion.json` (`{contractVersion: 1, steps}`) covers its folder and everything
  below it, and a deeper one replaces a shallower one.

## What OpenDAM has today

Surveyed 2026-10-22 on `main` `ed9be51`:

| # | Need | What exists | Gap |
|---|---|---|---|
| 1 | Collections to bundles | `collections.parent_id` nesting and smart collections (`filter_spec`). Definitions live in master, and each library DB holds membership rows for its own assets, by asset UUID (`db::library_of_asset`). One collection can span libraries. | No export, no copy-out, no path-type setting. Settings are JSON rows in master `settings`, and only `autotag/settings.rs` types them. |
| 2 | assetName | Format groups (`structure::format_groups`): `group_id` is the main item's id, chosen GLB > glTF > FBX > Blend > OBJ. Metadata writes reach every member (`structure::group_members`). | No name field. A file moved outside the app gets a new UUID (the watcher deletes the row and inserts a new one), and loses its collections and metadata. |
| 3 | Texture roles | Nothing pairs textures with models. `textures.py` resolves only base color, at preview time, and keeps no paths. | The whole role graph. Real packs show what to infer from: Stylarts' `T_desk_D/_N/_AORM.png`, and Unity packs' `.mat` and `.meta` GUID graphs. |
| 4 | Normalization | The worker loads FBX with `assimp_py` into trimesh, for previews only. It doesn't compute bounds or read `UnitScaleFactor` or `UpAxis`. The detail panel has a three.js `ThreeViewer`. | Everything. Blender 5.2.2 is installed at `/usr/bin/blender`. CI (`ubuntu-latest`) has no Blender. |
| 5 | Texture conversion | The Rust `image` crate decodes rasters. The worker has Pillow and numpy. | No repacking, roughness inversion or green flip. |
| 6 | conversion.json | The watcher indexes `datapackage.json` and no other JSON file. | Reading and merging `conversion.json`. |
| 7 | Provenance | `PackInfo` has name, version, publisher, authors and source URL, from `datapackage.json`, the license header and `pack_overrides`. A licence is a `LicenseKind` (CC0, CC-BY, CC-BY-SA, MIT, Apache-2.0, OFL, unknown), plus the datapackage's raw SPDX id. The override is free text. | No origin, no sha256 anywhere, no pack slug. |
| 8 | v1 scope | assimp reads FBX meshes. | Takes, rig detection, the animation-only check. |

No JSON-schema validation exists. Neither Rust (`jsonschema`, `sha2`) nor the worker has the
libraries.

## Decisions

From the interview on 2026-10-22. "Human" means the answer you gave. Every answer in rounds 1-3
was the recommended option, except D34, which you rewrote.

| # | Question | Answer | Made by |
|---|---|---|---|
| D1 | What rewrites models | **Headless Blender**, run as its own process, with a script OpenDAM ships. | Human, round 1 |
| D2 | Which models are rewritten | **Every model**, and the step is recorded. | Human, round 1 |
| D3 | Byte-stable re-exports | **Cache converted files** in `.opendam/export-cache/`, keyed by source sha256, step settings and tool version. | Human, round 1 |
| D4 | Where pixel work runs | **Rust, with the `image` crate.** A PSD that needs converting is refused in v1. A PSD that needs no conversion passes through. | Human, round 1 |
| D5 | Schema copy and checks | **Copied into src-tauri with a SOURCE note** (repo, path, commit, sha256). Every manifest and conversion.json is validated with the Rust `jsonschema` crate. A test pins the copy, and a script re-copies it. | Human, round 1 |
| D6 | Root and destination | **Two settings: export root folder and root collection.** The bundle path is `<export root>/<root collection name>/<Domain>/`. | Human, round 1 |
| D7 | A domain's assets | **The domain and all its subcollections, flattened.** Textures join through materials. Smart collections can't be domains. | Human, round 1 |
| D8 | Where your corrections live | **Per asset in the library DB**, written to every group member, the same in every domain. | Human, round 1 |
| D9 | Default assetName | **Drop a known type prefix**, split words and PascalCase them. | Human, round 1 |
| D10 | Name collisions | **Flagged, and the export refuses** until you rename one. | Human, round 1 |
| D11 | Role inference sources | **FBX slots and references, then Unity `.mat` via `.meta`, then file-name suffixes, then same-stem images**, in that priority. | Human, round 1 |
| D12 | Pack origin | **Inferred and editable on the pack:** `.meta` files mean unity, a `conversion.json` step from uasset means unreal, else generic. See A1 for where it is inferred. | Human, round 1 |
| D13 | Facing | **A default per origin** (unity: as is, unreal: +X turned to +Z, generic: as is), **plus a per-model 90 degree override about Y**, shown with a forward arrow. | Human, round 1 |
| D14 | Normal-map convention | **A pack setting defaulting from origin** (unreal: DirectX, else OpenGL). A `_DX`/`_GL` suffix wins over it, and a per-texture override wins over both. | Human, round 1 |
| D15 | Licence `spdx` | **The datapackage's SPDX id as stated (LicenseRef-* included)**, else the LicenseKind's id, else an override that is an SPDX id, else null. | Human, round 1 |
| D16 | Pack id, packless assets | **The datapackage name if it is a valid slug, else the folder slugified**, with `-2` on a clash. **An asset in no pack is refused.** | Human, round 1 |
| D17 | Multi-input sourcePath | **sourcePath is the roughness file.** The step's settings name the other inputs. | Human, round 1 |
| D18 | Writing a bundle | **Stage, validate, diff against the previous manifest, then swap in by renaming.** | Human, round 1 |
| D19 | LODs and collision | **Drop collision meshes, keep LODs** with Unity's `_LODn` names. Bounds cover every kept renderer. | Human, round 2 |
| D20 | Rig | **none or generic set automatically. Humanoid is suggested** from bone names, and only you set it. | Human, round 2 |
| D21 | Animations | **The same transform as its avatarFrom model**, applied to the armature object. avatarFrom defaults by best bone match and is editable. An FBX with no mesh and at least one take is an animation. | Human, round 2 |
| D22 | Material names | **From the slot, prefixes dropped, PascalCased.** Duplicates within a model refuse. Editable. | Human, round 2 |
| D23 | Cutout | **Inferred and editable:** a Unity `.mat` with alpha clip, else a suggestion from BaseColor alpha below 0.5, else opaque. | Human, round 2 |
| D24 | Texture names | **One model: `<AssetName>`, or `<AssetName><MaterialName>` when that model has several textures in the role. Shared: its own stem, stripped.** Collisions refuse. | Human, round 2 |
| D25 | Other image formats | **jpg/jpeg/bmp/webp/gif become PNG** (a recorded step). `.tiff` is renamed `.tif`. dds/exr/hdr are refused. Nothing is resized. | Human, round 2 |
| D26 | Layouts in v1 | **ORM/AORM/ARM, split Roughness/Gloss + Metallic (+ lone AO), HDRP MaskMap, and URP-ready pass-through.** | Human, round 2 |
| D27 | UI | **A Game tab in the detail panel and an Export view per domain.** | Human, round 2 |
| D28 | Several domains | **One domain per export** in v1. | Human, round 2 |
| D29 | Accepted Blender | **The major.minor CI pins (5.2.x).** Any other version refuses the export. A setting holds Blender's path. | Human, round 3 |
| D30 | Driving Blender | **One headless process per model or animation**, JSON in and out, a few at a time, each with a timeout. Cache hits skip Blender. | Human, round 3 |
| D31 | Groups with no FBX | **Refused in v1.** The exported file is always the group's FBX. | Human, round 3 |
| D32 | Stored or inferred | **Only your edits are stored.** Inferred values are recomputed when shown and at export. | Human, round 3 |
| D33 | Step identity | **Blender steps: tool Blender, its version, by `opendam-normalize/<N>`. Texture steps: tool OpenDAM, `opendam-textures/<N>`, by the operation.** Each `<N>` is a constant in the cache key. | Human, round 3 |
| D34 | Testing with Blender | **CI installs the pinned blender.org 5.2.x tarball**, sha256-verified and cached (not apt, which is older), and runs the Blender-backed tests end to end, failing if Blender is missing. Fixture FBX files are made by a committed Blender script, and every manifest and conversion.json a test writes is validated against the schema. | Human, round 2 (Other) |

These are agent decisions, made while writing the plan. Each is reviewed at final review:

| # | Decision | Why |
|---|---|---|
| A1 | **Origin is inferred when a pack is read for export, not by the structure pass.** That happens when the Export view, the Game tab or the pack edit form asks, or at export. The check stats `<file>.meta` for the pack's indexed files and walks the pack for `conversion.json`. Your override lives in `pack_overrides`, as D12 says. | The watcher indexes neither `.meta` nor `conversion.json`, and the structure pass reads no file on a rerun (the "Structure is derived" invariant). Inferring at read time keeps both invariants. D12's intent is unchanged: the origin is inferred and editable on the pack. |
| A2 | **The Blender script is embedded in the app** (`include_str!`) and written to a temp file per run. | `make install` and the installed app need no new file. Its version is `opendam-normalize/<N>`. |
| A3 | **The UI reads model facts through the preview worker.** A new `fbx_facts` request (assimp) returns material slots in order, texture references by type, mesh and bone names, and take names. Blender's result at export is authoritative. A slot list that differs from the worker's is a problem that refuses the export, naming the model. | The Game tab needs facts in milliseconds, and Blender takes seconds per model. |
| A4 | **Source sha256s are memoized** in a library DB table `file_hashes(file_path, size_bytes, modified_at, sha256)`. | The Export view shows "changed" without hashing a whole domain each time. |
| A5 | **The exported FBX is the group's first FBX member**, preferring one in an `fbx(unity)` format folder, then by path ignoring case. | D31 says the group's FBX, but a group can hold two. |
| A6 | **A domain member that is neither a model, an animation nor an image is a problem that refuses the export** (audio, documents). | "What the manifest doesn't list leaves the game." Skipping members silently would drop them from the game without a word. |
| A7 | **When a group forms, the main item's export edits win** (`structure::apply_unions`, as for the main item's own fields). | Groups share metadata. |
| A8 | **A pack with no licence at all exports `{ "spdx": null, "name": "Unknown" }`.** | The schema requires `name`. |
| A9 | **The export cache is never pruned automatically in v1.** Settings has "Clear export cache". | Pruning safely needs every domain's latest export. Known gap. |

## The flow

```mermaid
flowchart LR
  LIB[Library files + datapackage.json + conversion.json + Unity .mat/.meta] --> INF[export::infer]
  ED[export_edits + pack_overrides] --> INF
  W[Preview worker: fbx_facts] --> INF
  INF --> PLAN[Export plan: assets, names, materials, steps]
  PLAN -->|uncached model or animation| BL[Headless Blender, one per job]
  PLAN -->|texture needing work| TX[Rust texture ops]
  BL --> C[(.opendam/export-cache)]
  TX --> C
  C --> ST[Staging folder beside the domain]
  ST --> V{Schema + contract checks + sha256}
  V -->|fail| P[Problems in the Export view, old bundle kept]
  V -->|pass, diff confirmed| SW[Rename swap]
  SW --> OUT[export root / DungeonGame / Domain]
  OUT -->|make art BUNDLE=...| GAME[Dungeon Game importer]
```

The export reads only the library and writes only the cache and the export root. It never
writes into the game's repository (contract ADR D1).

## Contracts

**C1. The pinned schema.** `apps/desktop/src-tauri/contract/asset-contract.schema.json` is a
byte copy of dungeon-game's `docs/asset-contract.schema.json`. Beside it, `SOURCE.md` names the
repository, the path, the commit (`05279adae1cf2151af12a3ef307f27eac695b1ad`) and the file's
sha256.

- **A test pins it:** `contract::tests` checks that the copy's sha256 equals the one in
  `SOURCE.md`, so an edit made here fails CI.
- **One way to change it:** `scripts/sync-asset-contract.sh <dungeon-game checkout> <commit>`
  copies the file at that commit and rewrites `SOURCE.md`. It is run only after the change has
  merged in dungeon-game.
- **Validation:** `export::contract` embeds the copy (`include_str!`). It validates every
  manifest before the swap, and every `conversion.json` it reads against `$defs/conversionFile`,
  with the `jsonschema` crate (Draft 2020-12, `unevaluatedProperties` and `format` checked).
- **OpenDAM mirrors the importer's own checks** that a schema can't express: unique names, a
  known pack for every asset, every material texture naming a texture asset with that role,
  every `avatarFrom` naming a rigged model, and exactly the listed files, each matching its
  sha256. So OpenDAM never writes a bundle the importer would refuse on its manifest.
- **The contract version** OpenDAM writes is `1`, a constant beside the copy.

**C2. Settings.** `export/settings.rs`, modelled on `autotag/settings.rs`, stores three master
`settings` rows:

- `export.root`: an absolute folder.
- `export.rootCollection`: a collection id.
- `export.blenderPath`: empty means `blender` on `PATH`.

The Settings dialog gains a **Game export** section with a folder picker, a picker for a
top-level, non-smart collection, Blender's path with its detected version, and "Clear export
cache". An export with the root or the root collection unset is refused, and so is one whose
Blender isn't 5.2.x (D29). The accepted series is the constant `BLENDER_SERIES = "5.2"`, and a
test checks that CI's pinned version starts with it.

**C3. Domains.** A domain is a direct child of the root collection.

- **Names:** its name must match `^[A-Z][A-Za-z0-9]*$`, and so must the root's, since it is a
  folder of the bundle path.
- **Members:** the members of the domain and of all its subcollections, deduplicated, across
  every library (D7). A smart collection can't be a domain or sit below one.
- **How each member is read** (members are grid items: a main item or a file on its own):
  - a format group with an FBX member is a **model**, or an **animation** when its FBX has no
    mesh and at least one take (D21);
  - a format group with no FBX is refused (D31);
  - an image is a standalone **texture**, with its role inferred from its suffix, else
    `BaseColor`;
  - anything else is refused (A6).
- **Textures** reached through a model's materials join the domain without being members. A
  texture that is both a member and used by a material is one asset.

**C4. Your edits.** A library DB table `export_edits(asset_id TEXT PRIMARY KEY, edits TEXT NOT
NULL)` holds JSON, written to every member of the asset's group in one transaction
(`structure::group_members`). `no_metadata_write_bypasses_group_members` covers it, and A7 gives
the main item's edits precedence when a group forms. It holds only what you set (D32):

- **For a model:** `assetName`, `rig`, `facingQuarterTurns` (0-3), and `materials`, keyed by slot
  name. Each slot can hold `name`, `surface` and `textures`, a map from role to texture asset id,
  where `null` means "no texture in this role".
- **For an animation:** `assetName` and `avatarFrom`, the model's asset id.
- **For a texture:** `assetName`, `role`, `layout` and `normalConvention`.

It is kept out of `metadata`, which `assets_fts` indexes. A file moved outside the app gets a new
id and loses its edits and its membership, as for all metadata today. The export's diff shows
that asset as removed before anything is written (C15).

**C5. Names.** `export::names::pascal(stem)` makes every default (D9, D22, D24):

- **The rule:** drop one leading type prefix (`SM_ SK_ S_ T_ M_ MI_ MAT_ Mat A_ AN_ P_ BP_`, any
  case). Split on `_`, `-`, spaces and lower-to-upper case changes. Capitalise each word, keep
  digits, and drop every other character. A result starting with a digit gets an `X` prefix. So
  `SM_arch01_broken` becomes `Arch01Broken`, and `Small_chest` becomes `SmallChest`.
- **A model's or animation's default** comes from its group's main item's stem (D31: the file is
  the FBX, the name is the main item's).
- **A material's default** comes from its slot name.
- **A texture's default** (D24): a texture used by one model takes that model's `assetName`, or
  `<AssetName><MaterialName>` when that model has more than one texture in the role. A texture
  used by several models in the domain takes its own stem, with prefix and role suffix dropped
  (`PolygonCity_Texture_01_A` becomes `PolygonCityTexture01A`).
- **Uniqueness:** two models or animations with one name, two textures with one name and role,
  or two materials of one model with one name, are each a problem naming both assets. The export
  refuses (D10).

**C6. Packs and provenance.**

- **Pack id** (D16): the `datapackage.json` `name` when it matches `^[a-z0-9][a-z0-9-]*$`, else
  the pack folder's name lowercased, with each run of other characters turned into `-` and
  `-` trimmed from the ends. On a clash within a manifest, the later pack by library order, then
  path, gets `-2`, `-3`.
- **The other pack fields:** `name` is the pack's shown name (`pack_holding`). `version`,
  `publisher` and `sourceUrl` come from `PackInfo` and your overrides, and are left out when
  absent.
- **The licence** (D15, A8): `spdx` is the first datapackage licence `name` that is an SPDX id
  or a `LicenseRef-` reference, else the `LicenseKind`'s SPDX id, else your override text when it
  is an SPDX id, else null. `name` is the stated title, the override text or the kind's name, else
  `Unknown`. `url` is the licence's http(s) URL, when it has one.
- **The origin** (D12, A1): your `pack_overrides.origin`, else `unity` when any indexed file in
  the pack has a `<file>.meta` beside it, else `unreal` when a `conversion.json` in the pack has a
  step with `from: "uasset"`, else `generic`.
- **The normal-map convention** (D14): `pack_overrides.normal_convention` (`opengl` /
  `directx`), else `directx` for unreal and `opengl` otherwise.
- **The pack edit form** gains Origin and Normal maps fields, each showing its inferred value.
  `pack_overrides` gains the two nullable columns.
- **Every asset** carries `pack` (its pack's id), `sourcePath` (its source file relative to the
  pack folder, with `/` separators, and for a texture made from several files, the first input
  per D17) and `sha256` (of the file written to the bundle, lowercase hex).
- **An asset in no pack** is refused, with "mark its folder as a pack" (D16).

**C7. Materials and roles.** `export::infer` builds each model's materials. The slot list comes
from `fbx_facts` for the UI and from Blender at export (A3). For each slot, the first source with
an answer wins (D11):

1. **The FBX's texture references** for the slot (diffuse/base color, normal, metalness,
   roughness, AO, emissive, height), resolved as `textures.py` resolves them (given path, then
   relative, then by name within the pack), bounded to the library and pack roots.
2. **For a unity pack, a Unity material:** the model's `.fbx.meta` `externalObjects` remap for
   the slot, else a `.mat` in the pack named like the slot. The `.mat` texture properties are
   resolved to files through the pack's `.meta` GUIDs: `_BaseMap`, `_MainTex` or `_BaseColorMap`
   is BaseColor; `_BumpMap` or `_NormalMap` is Normal; `_MetallicGlossMap` is MetallicSmoothness;
   `_MaskMap` is an HDRP MaskMap; `_OcclusionMap` is Occlusion; `_EmissionMap` or
   `_EmissiveColorMap` is Emission; `_ParallaxMap` or `_HeightMap` is Height. Alpha clip on
   (`_AlphaClip: 1`, Standard `_Mode: 1`, HDRP `_AlphaCutoffEnable: 1`) makes the slot cutout
   (D23).
3. **File-name suffixes** on images in the pack whose stem, with prefix and suffix dropped,
   matches the slot's or the model's (`_D _BaseColor _Albedo _Diffuse _Color`, `_N _Normal _NRM`,
   `_ORM _AORM _ARM`, `_MaskMap _Mask`, `_R _Roughness _Rough`, `_G _Gloss _Glossiness
   _Smoothness`, `_M _Metallic _Metalness`, `_AO _Occlusion`, `_E _Emissive _Emission`,
   `_H _Height _Disp _Displacement`, `_MetallicSmoothness _MS`).
4. **Images in the model's format group or folder** with the model's stem, as BaseColor.

- **Your edits** (C4) replace any inferred field.
- **Surface:** when no Unity material says cutout, a BaseColor whose alpha has any pixel below
  0.5 is shown as a cutout suggestion. You accept it with one click, and until then it exports
  opaque.
- **A material with no textures** exports `textures: {}`.
- **A texture shows** its role, its layout (C8) and, for a normal map, its convention.

**C8. Texture conversion.** `export::textures` turns each texture a material or member uses into
the files its roles need, at source resolution and bit depth (8 or 16 bit), as PNG unless it
passes through.

| Source layout | Inferred from | Output |
|---|---|---|
| URP-ready (BaseColor, Normal in OpenGL, MetallicSmoothness, Occlusion, Emission, Height) | Role suffix or a Unity material | The source file, byte for byte, when its format is png/tga/psd/tif. No step. |
| ORM / AORM / ARM (AO R, roughness G, metallic B) | `_ORM _AORM _ARM` | Occlusion (AO in R, G and B), and MetallicSmoothness (metal in R, G and B, `1 - roughness` in A) |
| Separate Roughness or Gloss, plus Metallic | `_R _Roughness` / `_G _Gloss _Smoothness`, plus `_M _Metallic` | MetallicSmoothness. Roughness is inverted, gloss is not, and a missing metallic map means metal 0. A lone AO map becomes Occlusion. |
| HDRP MaskMap (metal R, AO G, detail B, smoothness A) | `_MaskMap _Mask`, or `_MaskMap` in a Unity material | MetallicSmoothness and Occlusion |
| DirectX normal map | The pack convention, a `_DX`/`_GL` suffix, or your override (D14) | Normal with green flipped (`max - G`) |
| jpg, jpeg, bmp, webp or gif in any role | Its extension | The same pixels as PNG (D25) |
| `.tiff` | Its extension | The same bytes as `.tif`, no step |
| dds, exr, hdr, or a PSD that needs any operation | | A problem that refuses the export (D4, D25) |

- **One step per operation**, in this order: `to-png`, `repack-orm` / `repack-mask` /
  `merge-metallic-smoothness`, `invert-roughness`, `flip-normal-green` (D33).
- **Each step** is `{tool: "OpenDAM", toolVersion: "opendam-textures/<N>", by: <op>, from:
  <source extension>, settings}`. The settings name the layout, the output role and any extra
  input (`metallicSource`, `aoSource`: paths inside the pack, D17).
- **PNG output is deterministic** (fixed encoder settings, no timestamps), so the same input
  gives the same bytes.

**C9. Blender normalization.** `export::blender` runs the embedded script (A2) as
`<blender> -b --factory-startup -P <script> -- <job.json> <result.json>`, one process per model
or animation, at most `max(1, cores / 4)` at a time, each with a 120 s timeout. A timeout or
crash is a problem naming the asset (D30). The job names the input FBX, the output path, the
kind, `facingQuarterTurns` and, for an animation, the transform of the model it follows. The
script:

1. **Imports** the FBX with Blender's importer (units and axes as the file states them).
2. **Drops collision meshes** whose names start with `UCX_ UBX_ USP_ UCP_` or end with
   `_Collision` or `_COL` (any case). LOD meshes keep Unity's `_LODn` names (D19).
3. **For a model, applies one root transform:**
   - the origin's facing default (unreal: +X turned to +Z), then your quarter turns about Y (D13);
   - then a translation that puts the bounds' `min.y` at 0 and the X and Z centre at 0, measured
     over every kept mesh (a skinned mesh at its bind pose);
   - the transform is applied to the armature object when there is one, never to bones (D21).
4. **For an animation, applies the transform recorded for its avatarFrom model** (D21). It
   refuses when the animation's bone names aren't a subset of the model's.
5. **Exports an FBX**, with settings fixed in the script, such that Unity's import with Scale
   Factor 1, Convert Units on and Bake Axis Conversion off gives metres, Y up and +Z forward
   (contract D21). Takes keep their source names: actions are renamed back from Blender's
   `<Armature>|<Take>`, and only takes with animation curves are written.
6. **Re-imports the written file** and reports, in Unity's axes, its `bounds`, its material slots
   in order, its takes, its bone names, the dropped meshes, whether it has a mesh and an
   armature, and the transform it applied.

- **The result's checks:** its bounds must have `min.y` within 0.1% of the largest dimension
  from 0, and the X and Z centre likewise. Its takes must equal the source's take names from
  `fbx_facts`, and its slots the worker's slots (A3). Any mismatch is a problem naming the asset.
- **The manifest's model entry** takes `bounds` from the result. `rig` is `none` without an
  armature, else `generic`, unless you set `humanoid` (D20). `clips` are the takes, left out when
  there are none, and `materials` come from C7 in the result's slot order.
- **Humanoid is suggested** when the bone names match a Mixamo (`mixamorig:Hips`...), Unity
  humanoid or UE mannequin (`pelvis`, `spine_01`...) naming.
- **avatarFrom defaults** to the rigged model in the domain sharing the most bone names with the
  animation (D21).
- **The step** (D33) is `{tool: "Blender", toolVersion: "<x.y.z>", by: "opendam-normalize/<N>",
  from: "fbx", settings}`, with flat settings: `facingQuarterTurns`, `originFacing`, `pivotX`,
  `pivotY`, `pivotZ`, `droppedMeshes` (a count), and, for an animation, `followed` (the model's
  assetName).

**C10. conversion.json.** For each source file, `export::conversion` walks from the file's folder
up to its library root and takes the nearest `conversion.json`. A deeper one replaces a shallower
one (contract). Each file read is validated against `$defs/conversionFile` (C1). An invalid one
is a problem naming the file, and refuses the export. An asset's `conversion` is that file's
steps, in their order, followed by OpenDAM's own steps (C8, C9), oldest first. A pack's own,
untouched file (a pass-through texture under no `conversion.json`) has `conversion: []`. For a
texture made from several files, the first input's chain is used and the others are named in
settings (D17).

**C11. The export cache.** Each library has `.opendam/export-cache/`. An entry is keyed by the
sha256 of the source's sha256 (every input's, in order), the operation's settings as canonical
JSON, the tool and its version, and the script or texture constant `<N>` (D3, D33). The entry
holds the output file and, for Blender, the result JSON. A hit skips Blender and the pixel work,
so re-exporting an unchanged domain writes byte-identical files: only `exportedAt` changes in the
manifest. Source hashes are memoized in `file_hashes` (A4). The cache is cleared only from
Settings (A9).

**C12. The worker's `fbx_facts`.** `worker.py` gains an `fbx_facts` request, which loads the FBX
with `assimp_py`, as `model_meta` does, and returns:

- the material slots, in the scene's material order;
- each slot's texture references by type;
- the mesh names, and whether the file has any mesh;
- the bone names;
- the animation (take) names.

It reads no texture file, and follows the existing `library_root` rule. The Rust side caches the
answer per file's size and mtime for the session.

**C13. The Game tab.** The tab shows for an FBX model, an animation or an image.

- **Each value** shows whether it is inferred or yours, and has a reset to inferred.
- **A model** shows `assetName`, rig (with the humanoid suggestion) and facing: a ThreeViewer of
  the normalized GLB, which Blender writes beside the cache entry on demand ("Show as exported"),
  with a +Z arrow and quarter-turn buttons. It also shows each material: slot, name, surface, and
  for each role, a texture picker limited to the pack's images or "none".
- **An animation** shows `assetName`, `avatarFrom` and its takes.
- **A texture** shows `assetName`, role, layout and normal convention.
- **Problems** for the selected asset are listed at the top of the tab.

**C14. The Export view.** It opens from a domain collection's menu: "Export to game...".

- **"Check"** builds the whole bundle into staging (C15), with progress for each Blender job.
- **It shows** every asset: kind, assetName, pack and status (ready, changed, new, or problem).
  It shows the problems, each linking to its asset's Game tab, and the diff against the previous
  `manifest.json` in the destination: added, changed (sha256 or entry) and removed, by kind and
  assetName, with renames shown as a removal plus an addition.
- **"Export"** is enabled only when there are no problems. It re-checks that no source's size or
  mtime changed since the check, then swaps the bundle in (D18, D27). One domain per export
  (D28).

**C15. Writing a bundle.** For domain `D` under `<root>/<Root>/`:

1. **Builds** `<root>/<Root>/.D.staging-<uuid>/`, holding `manifest.json` and exactly the listed
   files, each copied from the cache or the source.
2. **Validates** the manifest against the schema and C1's checks, and re-hashes every staged
   file against its `sha256`.
3. **On "Export":** renames an existing `D/` to `.D.old-<uuid>/`, renames staging to `D/`, then
   deletes the old folder. If the second rename fails, the old folder is renamed back.
4. **Cleans up:** staging is deleted when the view closes or the check fails, and a leftover
   `.D.staging-*` or `.D.old-*` from a crash is deleted at the next check.

- **The manifest** is pretty-printed UTF-8 JSON. Assets are sorted by kind (model, texture,
  animation), then assetName, then role, and packs by id, so a diff of two exports shows only
  real changes.
- **`exportedAt`** is the local time with its offset (RFC 3339).
- **`exporter.version`** is the app's version (`CARGO_PKG_VERSION`).

**C16. Tests and CI** (D34).

- **Fixtures:** `apps/desktop/src-tauri/tests/fixtures/game-export/make_fixtures.py`, run with
  `blender -b -P`, writes a small library that is committed with its output:
  - a centimetre, Z-up FBX with an asymmetric nose on its front;
  - an Unreal-style FBX with a `UCX_` mesh, `_LOD0`/`_LOD1` and two material slots;
  - a skinned model with one take, and a mesh-less FBX with a take;
  - textures: an AORM PNG, a DirectX normal map, a JPG base color with a cut-out alpha, and a
    separate Roughness and Metallic pair;
  - a `conversion.json` in a folder, and a deeper one below it;
  - a Unity-style `.mat` and `.meta` pair;
  - a `datapackage.json` with a `LicenseRef-` licence.
- **Rust tests (nextest), without Blender:** names, inference, the texture operations
  (pixel-exact on tiny images), `conversion.json` merging, packs and licences, cache keys,
  staging and swap (including a failed swap), and the schema pin. Every manifest or
  `conversion.json` a test writes is validated against the copied schema.
- **Blender-backed Rust tests:** they export the fixture domain end to end and re-import every
  model. They check:
  - bounds, pivot, and that the nose is at +Z;
  - that the collision mesh is gone and the LOD names are kept;
  - take names, and that the animation follows its model;
  - a second export reusing the cache, byte-identical.
  They find Blender through `OPENDAM_BLENDER`. Locally they skip with a message when it is
  missing. Under `CI` they fail.
- **CI's test job** downloads the pinned `blender-5.2.x-linux-x64.tar.xz` from blender.org,
  verifies its sha256, caches it by version and sets `OPENDAM_BLENDER`.
- **Python:** a `pytest` file for `fbx_facts`.

## Problems that refuse an export

Each one names the asset (or file) and the rule it broke, and the Export view lists them all:

- The root or root collection is unset, a domain or root name isn't PascalCase, or the domain or
  one of its subcollections is smart.
- Blender is missing or isn't 5.2.x. A Blender job timed out or crashed.
- A member is in no pack. A model's group has no FBX. A member is neither a model, an animation
  nor an image.
- Duplicate names (models and animations, textures with one role, materials within a model). A
  name that isn't PascalCase.
- An unsupported or unconvertible texture (dds, exr, hdr, or a PSD needing work). A material
  texture that resolves to no file.
- An animation with no avatarFrom, an avatarFrom with no rig, or bones not a subset of its
  model's.
- Blender's slots or takes differ from `fbx_facts`. The normalized bounds miss the pivot rule.
- An invalid `conversion.json`. A manifest that fails the schema or C1's checks. A staged file
  whose sha256 differs.

## Build order

Mission Control slices this into tickets after the plan merges. The natural order, each step
shippable on its own:

1. **Contract and skeleton:** C1 (schema copy, pin test, sync script, validation), C2 settings,
   C3 domains, C6 packs and provenance, and C15's staging and swap. These export pass-through
   textures with sha256, provenance and `conversion: []`, plus the Export view's list, problems
   and diff (C14).
2. **Names and edits:** C4, C5, and the Game tab's name fields (C13).
3. **conversion.json:** C10.
4. **Materials and roles:** C12 `fbx_facts`, C7 inference, and the Game tab's material editor.
5. **Texture conversion:** C8 and C11 for textures.
6. **Blender normalization:** C9, C11 for models, the CI Blender install, and the fixtures
   (C16). Models enter the export here.
7. **Animations and rigs:** the rest of C9 (avatarFrom, follow transform, humanoid suggestion).

Until step 6, a domain holding a model is refused with "model export not built yet", so no
bundle ever carries an unnormalized model.

## Acceptance criteria

- **AC1.** The copied schema matches `SOURCE.md`'s sha256 (a test). `sync-asset-contract.sh`
  reproduces it from dungeon-game at the recorded commit.
- **AC2.** The fixture domain exports to `<root>/<Root>/<Domain>/`, holding `manifest.json` and
  exactly the listed files. The manifest passes the schema and C1's checks, and every file
  matches its sha256 (a Blender-backed test, in CI).
- **AC3.** Default names follow C5. A collision refuses the export, naming both assets, and an
  edit fixes it and survives a reindex.
- **AC4.** On the fixtures, inference gives the expected materials, roles, layouts, surfaces and
  conventions. Each edit replaces its inferred value, and a reset restores it.
- **AC5.** Normalized fixture models re-import with bounds within 0.1% of the expected metre
  sizes, `min.y` 0, the X and Z centre 0, and the nose toward +Z. The collision mesh is dropped,
  the LOD names are kept, and the take names are preserved.
- **AC6.** The animation fixture gets its model's transform, and an animation whose bones don't
  match is refused.
- **AC7.** The texture operations are pixel-exact on test images: ORM split, roughness
  inversion, gloss pass-through, MaskMap split, green flip, and JPG to PNG. Each records C8's
  steps.
- **AC8.** An asset's `conversion` holds its nearest `conversion.json`'s steps, then OpenDAM's.
  An invalid `conversion.json` refuses the export.
- **AC9.** The pack id, licence (a `LicenseRef-` kept, Unknown with null), origin, sourcePath and
  sha256 follow C6.
- **AC10.** A second export of the unchanged fixture domain runs no Blender job and writes
  byte-identical files.
- **AC11.** A failed check leaves the previous bundle untouched. The diff lists added, changed
  and removed assets before "Export".
- **AC12.** CI's test job installs the pinned Blender and runs the Blender-backed tests, and
  fails when Blender is missing.
- **AC13 (by hand, not an agent's).** The fixture bundle that OpenDAM writes passes dungeon-game's
  `make art BUNDLE=<path>` once the importer exists. This confirms the axes and units end to end.
  The ticket that carries it writes "Part of", not "Closes".

## Known gaps

- **Origin is inferred at read time (A1).** A pack shows its inferred origin only once it is
  read for export, not in the pack panel before.
- **Unity's Standard "smoothness from albedo alpha"** (`_SmoothnessTextureChannel: 1`) isn't
  split out in v1. Such a material's MetallicSmoothness passes through without its smoothness.
  Shader Graph materials with custom property names fall back to suffixes.
- **A PSD that needs any operation is refused** (D4).
- **The export cache isn't pruned** automatically (A9).
- **A file moved outside the app** loses its edits and its domain membership, like all metadata.
  The export's diff shows it as a removal.
- **The axes are proven in Unity only by AC13**, which needs the game's importer and Unity. The
  Blender tests check the axes against the contract's mapping, not against Unity itself.

## Out of scope

Audio, glTF and GLB models, Animator Controllers, a licence gate (contract "Out of version 1"),
exporting several domains at once (D28), moving the per-pack conversion scripts into OpenDAM
(contract: later), and any change to the contract.

## Risks

- **Blender's FBX round trip.** Re-exporting can rename things or bake transforms
  unexpectedly. The script re-imports its own output, and its slots and takes are checked
  against the worker's. The bounds and pivot rules catch scale and axis mistakes before the
  importer does.
- **Unity's axis conversion.** The mapping from Blender's output to "Unity's axes" in C9 is the
  one most likely to be wrong in a way only Unity shows. AC13 is the guard, and the nose fixture
  makes a wrong turn visible.
- **Pinning Blender.** Your distro's Blender will move past 5.2. Then an export refuses until
  `BLENDER_SERIES` and CI are bumped together, or `export.blenderPath` points at a 5.2 tarball.
- **New crates.** `jsonschema` and `sha2` add to the Rust build.
