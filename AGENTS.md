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
| `make test` | `bun test` and `cargo nextest run`, both writing JUnit XML (see below) |
| `make build` | Release build without bundling (`tauri build --no-bundle`) |
| `make install` | `make build`, then installs `~/.local/bin/opendam`, a desktop entry, an icon and the preview engine under `~/.local/share/opendam/preview-engine` |
| `make uninstall` | Removes exactly what `make install` created, never the master DB or a library's `.opendam/` |

CI runs `make deps check lint build` and, in a separate job, the same bun test and nextest runs as `make test`, as separate steps so failed TypeScript test files are rerun once to tell flakes from failures (`.github/actions/mission-flake-report`, settings in `.mission/testing.json`). Pull requests that change only Markdown, `docs/`, `.gitignore` or `LICENSE` skip both jobs, which still passes the required check. Formatting is enforced: run `bun run format`,
`cargo fmt` (in `apps/desktop/src-tauri`) or `uv run ruff format` (in `packages/preview-engine`)
before committing.

To run, inspect or screenshot the app, read [docs/agents/running-the-app.md](docs/agents/running-the-app.md).

## Layout

```
apps/desktop/                 Tauri app (bun workspace @opendam/desktop)
  src/                        React frontend: components/, store/ (Zustand), api/ (IPC wrappers), types/, utils/
  src-tauri/
    src/commands/             Tauri IPC commands (asset, autotag, collection, library, preview, settings, shell, tag)
    src/autotag/              AI auto-tagging: settings, keyring credentials, providers, run engine and run tables
    src/db.rs                 Master DB + per-library SQLite pools and pragmas
    src/watcher.rs            Directory indexing and change detection
    src/preview.rs            Rust raster thumbnails + the persistent Python worker bridge
    migrations/               Library DB schema
packages/preview-engine/      Python worker (worker.py), uv project, dev venv in .venv/
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
  requests over the existing stream; one process serves every file.
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

- **Structure is derived.** `folders` (each folder's path, parent, name and recursive
  `item_count`, the root being `path = ''`) is recomputed by `structure.rs` from the indexed
  paths: `structure::derive` is a pure function, and `structure::apply` writes only the rows that
  changed, in 200-row transactions. Every `watcher::index_paths` run ends with the pass, under the
  library's `Database::index_lock`. Only `structure.rs` writes `folders`: a command that changes
  the indexed paths (`delete_asset`, `delete_assets`, `rename_asset`) calls
  `structure::rebuild_library` before it returns, rather than patching rows, and that waits for
  any index run holding the lock.

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
  - JUnit: bun writes `apps/desktop/test-results/junit.xml`, whose `file` attribute names each
    test's file. nextest writes `apps/desktop/src-tauri/target/nextest/ci/junit.xml`, which names
    no file: `classname` is the test binary and `name` the module path, so `db::tests::x` in
    `opendam-desktop` is in `src/db.rs`.
- **Commit attribution:** commits are authored by the human owner alone. Never add an agent as a
  co-author: no `Co-Authored-By` trailer naming Claude or any other agent, whatever a harness's
  default attribution says. This applies to every commit, including squash and merge messages.
- **Writing style:** use plain dashes (`-`) only, never em dashes, in code, comments, docs, commit
  messages and PR text.
