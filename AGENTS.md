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
| `make dev` | Runs the app with hot reload (`tauri dev`) |
| `make lint` | oxlint, oxfmt check, `cargo fmt --check`, clippy (`-D warnings`), ruff format check, ruff check |
| `make check` | Tauri crate/npm version match, `tsc`, `cargo check` |
| `make test` | `bun test` and `cargo nextest run`, both writing JUnit XML (see below) |
| `make build` | Release build without bundling (`tauri build --no-bundle`) |

CI runs `make deps check lint build` and, in a separate job, the same bun test and nextest runs as `make test`, as separate steps so failed TypeScript test files are rerun once to tell flakes from failures (`.github/actions/mission-flake-report`, settings in `.mission/testing.json`). Formatting is enforced: run `bun run format`,
`cargo fmt` (in `apps/desktop/src-tauri`) or `uv run ruff format` (in `packages/preview-engine`)
before committing.

To run, inspect or screenshot the app, read [docs/agents/running-the-app.md](docs/agents/running-the-app.md).

## Layout

```
apps/desktop/                 Tauri app (bun workspace @opendam/desktop)
  src/                        React frontend: components/, store/ (Zustand), api/ (IPC wrappers), types/, utils/
  src-tauri/
    src/commands/             Tauri IPC commands (asset, collection, library, preview, shell, tag)
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
- **One persistent Python worker.** The worker is a long-lived JSON-over-stdio daemon. Send it
  requests over the existing stream; one process serves every file.
- **Zero telemetry.** The app makes no network calls to analytics or tracking services.
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

## Python preview worker

`make deps` creates the dev venv at `packages/preview-engine/.venv` with `uv sync --frozen`.
Dependencies live in `pyproject.toml` and `uv.lock`: change them with `uv add`/`uv remove` in that
directory and commit the lockfile. The app finds the worker through `OPENDAM_PYTHON` and
`OPENDAM_WORKER` if set, otherwise by searching for `packages/preview-engine/.venv/bin/python`
from its working directory and executable directory upward.

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
