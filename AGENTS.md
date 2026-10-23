# OpenDAM

A personal, Linux-only desktop asset manager for game-dev assets (glTF/FBX/OBJ models, PNG sprites
and textures). It is a hard fork of [LawLaw09/OpenDAM](https://github.com/LawLaw09/OpenDAM), an
AEC/archviz DAM: it diverges freely and ports nothing back. Agents dispatched by Mission Control
build and maintain it. The app keeps the name "OpenDAM" and the identifier `com.opendam.desktop`.

Stack: Tauri v2 (Rust backend), React 18 + TypeScript + Vite + Zustand frontend, SQLite (sqlx,
FTS5), and a Python preview worker for formats Rust can't decode.

## Commands

Set up a fresh checkout or worktree with `mise install && make deps`. `mise.toml` pins bun, rust,
uv and cargo-nextest; uv pins Python from `packages/preview-engine/.python-version`.

| Command | What it does |
|---|---|
| `make deps` | `bun install` and `uv sync`, rerun only when a manifest or lockfile changes |
| `make dev` | Runs the app with hot reload (`tauri dev`), under a memory cap (see [Memory-heavy tools](#memory-heavy-tools)) |
| `make lint` | oxlint, oxfmt check, `cargo fmt --check`, clippy (`-D warnings`), ruff format check, ruff check |
| `make check` | Tauri crate/npm version match, `tsc`, `cargo check` |
| `make test` | `bun test` and `cargo nextest run`, both writing JUnit XML (see below), and the preview engine's `pytest` |
| `make build` | Release build without bundling (`tauri build --no-bundle`) |
| `make install` | `make build`, then installs `~/.local/bin/opendam`, a desktop entry, an icon and the preview engine under `~/.local/share/opendam/preview-engine` |
| `make uninstall` | Removes exactly what `make install` created, never the master DB or a library's `.opendam/` |

CI runs `make deps check lint build` and, in a separate job, installs the Blender that
`.github/blender.env` pins (`scripts/install-blender.sh`, sha256-checked and cached) and runs the same bun test, nextest and pytest runs as `make test`, as separate steps so failed TypeScript test files are rerun once to tell flakes from failures (`.github/actions/mission-flake-report`, settings in `.mission/testing.json`). Pull requests that change only Markdown, `docs/`, `.gitignore` or `LICENSE` skip both jobs, which still passes the required check. Formatting is enforced: run `bun run format`,
`cargo fmt` (in `apps/desktop/src-tauri`) or `uv run ruff format` (in `packages/preview-engine`)
before committing.

To run, inspect or screenshot the app, read [docs/agents/running-the-app.md](docs/agents/running-the-app.md).

## Layout

```
apps/desktop/                 Tauri app (bun workspace @opendam/desktop)
  src/                        React frontend: components/, store/ (Zustand), api/ (IPC wrappers), types/, utils/
  src-tauri/
    src/commands/             Tauri IPC commands (asset, autotag, collection, export, library, preview, settings, shell, tag)
    src/autotag/              AI auto-tagging: settings, keyring credentials, providers, run engine and run tables
    src/export/               Game export to Dungeon Game bundles: contract (schema validation and the
                              importer's other rules), settings, the export cache, check_outside_libraries,
                              which refuses an export root equal to, inside or holding a library root,
                              after resolving symlinks, blender.rs, which finds Blender and gates it to
                              BLENDER_SERIES (5.2), and check, the Export view's Check over a domain
                              (names, packs, textures, conversion and hashes, memoized in file_hashes,
                              and FBX models and animations by fbx_facts, which asks the preview
                              worker and caches its answers, with collision::PATTERNS, the one list
                              of collision meshes the export drops), infer, which groups a
                              pack's images into texture sets and infers each model's materials,
                              and unity, which reads a Unity pack's .meta and .mat files
    tests/fixtures/game-export/  The export's fixture library, made by make_fixtures.py under Blender and
                              committed with its output, and fbx_facts.json, its FBX files' facts,
                              which test_fbx_facts.py checks and the Check's tests read
    src/db.rs                 Master DB + per-library SQLite pools and pragmas
    src/watcher.rs            Directory indexing and change detection
    src/preview.rs            Rust raster thumbnails + the persistent Python worker bridge
    migrations/               Library DB schema
    contract/                 Pinned copy of dungeon-game's asset contract schema, never edited by hand:
                              SOURCE.md pins it, scripts/sync-asset-contract.sh re-copies it
packages/preview-engine/      Python worker (worker.py, fbx_nodes.py: an FBX node-tree reader), uv project, dev venv in .venv/
docs/adr, docs/specs          Upstream design records, kept as history
docs/plans                    Plans for this fork
.github/                      CI workflow, Dependabot, Tauri version check script
```

## Architecture invariants

- **Relative paths in library DBs.** Every path stored in `.opendam/library.sqlite` is relative to
  the library root (`textures/rock.png`, `.opendam/thumbs/...`), so a library folder works wherever
  it is mounted.
- **`DELETE` journaling on library DBs.** Library DBs use rollback journaling because WAL's `-shm`
  locking is unreliable on network shares. Keep `SqliteJournalMode::Delete` in `db.rs`.
- **Batched writes.** Database writes go in transactions of about 200 rows, as `watcher.rs` does.
- **Rust-native raster decoding.** Standard rasters (jpg, png, tga, tif, bmp, webp) are decoded in
  Rust with the `image` crate. Only formats Rust can't decode go to Python.
- **No upscaled thumbnails.** `generate_image_thumb` reads a raster's size from its header first.
  Sources 512 px or smaller that the webview can display (png, jpg, jpeg, webp, bmp, gif) get no
  thumbnail, and the UI shows the original, chosen by extension (`utils/media.ts`). Smaller TGA
  and TIFF get a PNG at their own size. A changed raster gets a fresh preview job, and a job that
  clears or renames a thumbnail deletes the old `_thumb` file before it updates the row. Libraries
  made before this rule lose their upscaled thumbnails once, in the background, through
  `thumb_cleanup.rs`, which records `thumbs_no_upscale` in the library's `library_meta` table.
- **One persistent Python worker.** The worker is a long-lived JSON-over-stdio daemon. Send it
  requests over the existing stream; one process serves every file. The queue starts it eagerly
  and asks each new process its `engine_info`, publishing the answer in
  `AppState::engine_version` (`preview::PreviewEngine`). Replies are matched to requests by `id`.
  Other parts of the app, such as the game export's `fbx_facts`, send a `preview::WorkerRequest`
  to the queue, which serves it on that process between jobs.
- **Zero telemetry.** The app makes no network calls to analytics or tracking services. The only
  outbound calls are AI tagging requests to the provider you configured (the Anthropic API, your
  local `claude` CLI, or TypeSafe's Jev). They are made only for runs you start, and nothing is
  sent until a provider is set up.
- **Claude-plan access is personal use only.** Anthropic's "Claude plan" access runs the owner's
  own installed, logged-in `claude` CLI (`autotag/claude_cli.rs`). Anthropic doesn't let
  third-party products offer claude.ai login or plan limits, so this access mode must be removed
  or disabled before the app is ever distributed. Its child process starts from an empty
  environment holding only `claude_cli::CHILD_ENV`, so no API key or other auth override set for
  the app reaches it. Keep it that way.
- **Deterministic search order.** Every search `ORDER BY` in `commands/asset.rs` ends with
  `a.rowid` in the sort direction, and the name sort is `COLLATE NOCASE`, so offset pages never
  overlap or skip, even across assets that share a sort key. Across libraries, ties break by the
  library's position in `get_all_libraries` order, then `rowid`, and the merge compares keys
  exactly as SQLite does (`sort_column` names each field's `SortKind`; NOCASE folds only ASCII).
- **Windowed results.** The grid and the list render only the visible rows plus overscan, from
  `useAssetStore`'s page cache: 200-asset pages by index, at most 25 kept, with selected and
  focused assets pinned. Nothing maps over all results. Change a cached asset through
  `patchCached`, and refetch after anything that shifts indices (`refresh`).
- **No `backdrop-filter` in the grid or the list.** Under GPU rendering (`gpu-shm` on NVIDIA) each
  one is a compositing layer, and once the grid scrolls WebKitGTK 2.54's compositor re-renders
  them without end, growing the web process about 500 MB/s. `gridStyles.test.ts` keeps them out
  of `components/AssetGrid/*.css`, and the scroll bench's M1 check measures the web process held
  still deep in the grid.
- **Master DB is the tag and collection catalog.** Tag and collection lists are read from master.
  Definition create, update and delete write to master, then mirror to every open library DB.
  `asset_tags` and `collection_assets` rows go only to the asset's own library
  (`Database::library_of_asset`). There is no "active library": every caller names a library or
  iterates all of them (`get_all_libraries`, in library-id order). When libraries open
  (`Database::sync_open_libraries`), master first gains the definitions it lacks from every open
  library (insert-only), then every mirror is overwritten from master and drops memberships for
  assets its library doesn't hold. Mirror writes update rows in place (`db::upsert_tag`,
  `db::upsert_collection`), never `REPLACE`, which cascade-deletes memberships and child
  collections.

- **Structure is derived.** `folders` (each folder's path, parent, name, `is_pack`, and its
  recursive `item_count`, `hidden_count` and `split_groups`, the root being `path = ''`, and a
  pack's name, cover, gallery, license, `datapackage.json` and files by kind in `pack_json`) and
  `assets.support_role`, `assets.group_id` and `assets.bundled_preview` are recomputed by
  `structure.rs` from the indexed paths and sizes: `structure::derive` is a pure function, and
  the pass writes only the rows that changed, in 200-row transactions. The pass reads a pack's
  license file (`structure::parse_license`) only when the file's size or mtime changed since it
  last read it, or an older `LICENSE_PARSER` read it, and a pack's `datapackage.json`
  (`structure::parse_package`, at most 1 MiB) on the same terms under `PACKAGE_PARSER`, and a
  render's header only when it has no read recorded or its size or mtime changed since, so a
  rerun reads no file. A `datapackage.json` (any ASCII case, `structure::is_package_file`) is the
  only JSON file the watcher indexes: a Document whose folder is a pack, as if marked, unless
  it is "Not a pack". A malformed one is ignored with its read recorded, and one it can't read
  records none. It caches a
  render's size on the render's own row: `render_width` and `render_height`, with the
  `size_bytes` and `modified_at` it read them at in `render_read_bytes` and `render_read_mtime`.
  A header it can't read sets all four to `NULL`, so the next pass reads it again. Opening a
  library DB (`connect_library_sqlite`) runs the pass once, and
  every `watcher::index_paths` run ends with it, under the library's `Database::index_lock`. Only
  `structure.rs` writes `folders`, `support_role`, `group_id`, `bundled_preview` and the
  `render_*` columns: a command
  that changes the
  indexed paths (`delete_asset`, `delete_assets`, `rename_asset`) calls
  `structure::rebuild_library` before it returns, rather than patching rows, and that waits for
  any index run holding the lock. Searches hide support files (`support_role IS NOT NULL`) unless
  the filter's `showHidden` is set.
  - Your pack edits ("Mark as pack", "Not a pack", a pack's name, license, source URL and cover,
    and the game export's origin and normal-map convention) live only in `pack_overrides`, keyed by the folder's relative path, so they outlive the
    folder going away and coming back. The pass reads them (`structure::read_overrides`) and
    never writes or deletes them. `set_pack_override` writes one, then calls
    `structure::rebuild_library` before it emits `library:updated`, like the commands above.
    Each field falls back on its own, and your edits win: a pack's name is yours, else the
    `datapackage.json` `title`, else the license header's, else the folder's; its cover is yours,
    else the file's `image`, else its first `Preview*` image; its license is yours, else the
    file's, else the license file's; and its source URL is yours, else the file's. Version and
    authors come from the file, else the license header, and the publisher only from the file
    (`pack_holding`). The file's license is its first entry with a `name`, an http(s) URL, or a
    `path` that is an indexed `txt`, `md` or `pdf` file in the pack (`PackageFile::license`).
    "Open license" opens that file, else the pack's license file; a license URL opens with
    "Open", and no URL is fetched until you click.
- **One grid item per group.** A format group (one model in several formats, or a file's copies
  under format folders, plan C11) holds its main item's id in `group_id` on every member, the
  main item included; a file on its own has `NULL`. The main item is chosen GLB > glTF > FBX >
  Blend > OBJ > others, ties by path ignoring case.
  - A 2D stack (plan C13) is a group too: a sprite's scale and format copies (`Default/`,
    `Double/`, `Vector/`, the table in `structure.rs`), among images in no format group and no
    support files. Style and color folders (`Dark`, `Blue`) aren't in the table, so their
    sprites stay apart. Its main item is a PNG first, then the default scale, then by path.
  - The grid shows main items and files on their own, never other members, even with
    `showHidden`, and support files only with it (`MAIN_OR_SINGLE` and `NOT_SUPPORT` in
    `commands/asset.rs`). The partial index `idx_assets_items` repeats those two terms in its
    `WHERE`, so keep them term for term alike. `preview::claim_jobs` uses `MAIN_OR_SINGLE` itself.
  - File-level filters (search text, kind, extension, folder, name prefix and suffix) match a
    group when one of its files meets all of them together; metadata filters and sort keys read
    the main item. A folder whose `split_groups` is 0 holds every group it has a file of together
    with its main item, so a search for it reads the item's own path, which keeps it as fast as a
    flat search.
  - A folder's `item_count` counts a group once under each folder holding one of its files, so
    it equals the grid's total for that folder. The live counts shown while search text or a
    filter is on (`get_folder_counts`, `get_facet_counts`) follow the same rule: a group counts
    once toward each folder, kind and extension among its files that meet every file-level
    filter, so each count equals the grid's total once you select that folder or value. A format
    pill adds its extension to the active ones, so with extensions active, its count also holds
    the items they match (`FacetCounts::extension_base`).
  - Metadata (rating, color label, favorite, description, tags, collections) is the same on
    every member. Each write expands its asset id through `structure::group_members` in its own
    transaction, which `no_metadata_write_bypasses_group_members` checks, and auto-tag sends
    only main items. A library or folder run sends the grid's items below it, never support
    files: `asset::folder_item_ids` runs the grid's own search for the folder. The bulk folder
    writes (`tag_folder`, `collect_folder` in `commands/folder.rs`) expand a folder through
    `folder_members` instead, which the check also accepts: under the library's index lock, they
    commit about 200 files per transaction, cut only between groups (`group_batches`), and
    re-read the tag or collection for each batch under `Database::catalog_lock`, which every
    definition update and delete also takes. When a group forms or grows, the structure pass
    gives every member the union of the group's tags and collections and the main item's fields
    (`apply_unions`).
  - Delete and rename reach every member and the bundled preview, unless another item shows the
    same render (`item_files` in `commands/asset.rs`). A rename gives them all the new stem,
    refuses before anything moves when a name is taken, and puts files back if a step fails.
  - The preview queue skips members other than the main item, and their thumbnails stay on disk.
    An index run queues no new file itself: the pass it ends with queues every file but those
    members (support files' cover images included), so a member is never queued, even during a
    library's first index. The pass also takes a file that has become a member off the queue,
    and `claim_jobs` skips members. One exception: a group whose main item takes no preview,
    such as a `.blend`, keeps its best previewable member queued as a stand-in
    (`structure::preview_stand_ins`, `preview::claim_jobs`'s `stands_in`), and its card shows
    that member's thumbnail. `preview.rs` owns the job
    states and how an asset goes on or off the queue (`preview::enqueue`, `preview::unqueue`).
- **Bundled previews.** An image in a folder named `Previews`, `Preview` or `Renders` (any case)
  renders each model below that folder's parent with the same stem, ignoring case (plan C12).
  The pass makes it a `model_preview` support file and stores its path in `bundled_preview` on
  the model's main item, or on the model when it is on its own. A render **fits** a card when
  both of its cached sides are at least `preview::THUMB_SIZE` (512 px). One of unknown size,
  never read or unreadable, doesn't (`structure::fits`). An item rendered more than once names
  its first render by path that fits, else its first by path, and a render too small to fit
  stays a hidden `model_preview` that moves with its model (plan small-bundled-previews C1-C3).
  An image that renders no model,
  or that the webview can't display itself (`preview::is_webview_displayable`), stays an
  ordinary image, so a render without a thumbnail still shows. `ASSET_COLUMNS` sends an item's
  render as `bundledPreview` when the item is pictured (below), else as `fallbackRender`, each as
  the render's own thumbnail when it has one, else the render itself. Cards and list rows show
  the bundled preview first, then the thumbnail, then the fallback render, then the original,
  then an icon, and move to the next when one fails to load (`tileMedia`), so a model whose
  render is too small shows that render only until it has its own thumbnail. The detail panel
  and the pack gallery show neither. `preview:done` carries the
  finished file's `filePath` and its `previousThumbnailPath`, so cached models showing either as
  their bundled preview or fallback render switch to its new thumbnail, or to the render itself
  when the thumbnail was cleared (`updateAssetPreview`). An item is **pictured** when its bundled
  preview fits. The
  pass keeps a pictured item off the preview queue and gives its group no stand-in, and
  `claim_jobs` skips its jobs. An item whose render is too small, or that loses its render,
  goes back on the queue unless it already has a preview, and a group whose main item takes no
  preview gets its stand-in. The Rust side is `structure::Structure::pictured`, and every SQL
  check is the one fragment `commands::asset::pictured`.
- **Textured model previews.** FBX and 3DS previews carry their base color textures, found by
  `packages/preview-engine/textures.py` (plan textured-fbx-previews).
  - **Inside the library, bounded by the pack.** `model_meta` gets `library_root` and, inside a
    pack, `pack_root` (`preview::model_meta_request`). The worker reads texture files only inside
    `library_root`, follows no symlink out of it, and searches by name only within the pack, or
    2 levels up without one. A request without a `library_root` that contains the model reads no
    texture file at all.
  - **At most 1024 px.** Every texture in a preview GLB is downscaled to 1024 px on its longest
    side (`MAX_SIDE`), FBX/3DS and trimesh-branch (OBJ, glTF, GLB) alike: JPEG when opaque, PNG
    when its alpha is a cutout or, in the trimesh branch, any alpha.
  - **A texture failure never fails a preview.** A missing, unreadable or too large texture
    leaves the material its color, and the preview still succeeds.
  - **Preview changes reach existing libraries through a version.** `worker.py` declares
    `PREVIEW_VERSION`, each `model_meta` result carries it, and `complete_job` stores it in
    `assets.preview_version`. When previews improve, bump it together with
    `model_preview_regen::MODEL_PREVIEW_VERSION` (a Rust test keeps the two equal): the
    `model_preview_version` pass in `library_meta` then requeues the stale `done` FBX/3DS items
    the queue rules allow, rather than a migration or a manual Re-generate.
  - **Bookkeeping stays out of `metadata`.** `preview_version` and the `preview_textures` report
    have their own `assets` columns (`db::add_preview_columns`). `metadata` is indexed by
    `assets_fts`, and the report's words and texture names would make searches match.
  - **The pass trusts only the running worker.** It requeues only while the worker process
    serving the queue has declared, through `engine_info`, a version of at least
    `MODEL_PREVIEW_VERSION`, never from a file on disk. Before any answer it returns silently,
    and below the constant it requeues nothing and warns.
  - **The key waits for the work.** The pass records `model_preview_version` only once no stale
    FBX/3DS preview is left, done or waiting, so jobs an older process served are redone later.
  - `packages/preview-engine/tools/measure_textures.py` measures all of this on a real library
    without writing to it. Run it under `scripts/memory-cap.sh`.
- **Source hashes are a memo.** `file_hashes` (library DB) holds a sha256 per file path,
  relative to the library root, with the size and mtime (nanoseconds) it was taken at. Only the
  game export's Check writes it (`export::hashes`): a row is used while the file keeps that size
  and mtime, else the file is hashed again, and fresh hashes are saved in 200-row transactions
  after the Check reads its files. Nothing prunes rows of files that went away, so never read a
  row as proof a file exists, and don't treat the table as authoritative: it can always be
  dropped and refilled.

## Python preview worker

`make deps` creates the dev venv at `packages/preview-engine/.venv` with `uv sync --frozen`.
Dependencies live in `pyproject.toml` and `uv.lock`: change them with `uv add`/`uv remove` in that
directory and commit the lockfile. The app finds the worker in this order (`find_preview_worker`
in `preview.rs`):

1. `OPENDAM_PYTHON` and `OPENDAM_WORKER`, if set.
2. The installed engine, `${XDG_DATA_HOME:-~/.local/share}/opendam/preview-engine`.
3. `packages/preview-engine` in a checkout, searched from the working directory and the executable
   directory upward.

The engine's `.venv/bin/python` runs `worker.py`, or `python3` on `PATH` if it has no `.venv`.
After `make install`, `make dev` also runs the installed engine, so to test changes to `worker.py`,
run `make uninstall` or set `OPENDAM_WORKER` and `OPENDAM_PYTHON` to the checkout's files. The
worker's stderr goes to the app log under the `preview_worker` target.

## Memory-heavy tools

Agent processes share Mission Control's cgroup, so one runaway process can get the whole session
OOM-killed. That happened four times on 2026-10-17 (02:44, 10:41, 12:03 and 15:39), when the scroll
bench's WebKit web process reached 15 to 20 GB in `checks` mode. The cause was the grid cards'
`backdrop-filter` (see "No `backdrop-filter` in the grid or the list"), but keep the cap.

- **Run them under `scripts/memory-cap.sh`.** It runs a command in its own systemd user scope
  with `MemoryMax` (`OPENDAM_MEMORY_MAX`, default `10G`) and no swap, keeps its exit status and
  says when the cap killed it (status 137). Without `systemd-run --user` it warns and runs the
  command uncapped. `bun run dev` and `bun run build` (so `make dev`, `make build` and
  `make install`) and the app inside `scroll-bench.sh` already go through it. Wrap anything else
  that starts the app or a webview, such as `./src-tauri/target/debug/opendam-desktop`, yourself.
- **Never run two benchmarks at once.** Other worktrees run them too: check that
  `pgrep -f '^bash .*scroll-bench\.sh'` finds nothing first.
- **A capped run that gets OOM-killed is a finding, not a flake.** Report it, with the scenario
  that was running. Don't rerun it in a loop or raise the cap to get past it.

## Working rules

- **Branches:** `feat/<topic>`, `fix/<topic>` or `chore/<topic>`, created from an up-to-date
  `origin/main`.
- **Commits:** [Conventional Commits](https://www.conventionalcommits.org/) (`feat: ...`,
  `fix: ...`, `chore: ...`, `docs: ...`, `refactor: ...`, `test: ...`).
- **Never merge.** Push the branch and open a pull request; the owner merges.
- **Tests run by file.** Locally, run only the tests you added, changed or are fixing, by file or
  by name, and leave the full suite to CI's test job:
  - TypeScript: `cd apps/desktop && bun test src/utils/format.test.ts` (or `-t '<name>'`). Tests
    sit beside the code as `*.test.ts`.
  - Rust: `cd apps/desktop/src-tauri && cargo nextest run db::tests` (a test-name filter). Unit
    tests live in a `#[cfg(test)] mod tests` in the module they test; `db::memory_library_pool()`
    gives a test an in-memory SQLite database with the library schema.
  - Blender-backed Rust tests find Blender through `OPENDAM_BLENDER`
    (`export::blender::test_blender`): without it they skip with a message, and under `CI` they
    fail. Run them with `OPENDAM_BLENDER=/usr/bin/blender cargo nextest run export::blender`.
    `BLENDER_SERIES` and the pin in `.github/blender.env` move together, which a test checks.
  - Python: `cd packages/preview-engine && uv run --frozen pytest tests/test_fbx_scene.py` (or
    `-k '<name>'`). Tests live in `packages/preview-engine/tests/` and import `worker` directly.
  - JUnit: bun writes `apps/desktop/test-results/junit.xml`, whose `file` attribute names each
    test's file. nextest writes `apps/desktop/src-tauri/target/nextest/ci/junit.xml`, which names
    no file: `classname` is the test binary and `name` the module path, so `db::tests::x` in
    `opendam-desktop` is in `src/db.rs`.
- **Commit attribution:** commits are authored by the human owner alone. Never add an agent as a
  co-author: no `Co-Authored-By` trailer naming Claude or any other agent, whatever a harness's
  default attribution says. This applies to every commit, including squash and merge messages.
- **Writing style:** use plain dashes (`-`) only, never em dashes, in code, comments, docs, commit
  messages and PR text.
