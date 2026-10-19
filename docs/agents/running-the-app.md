# Running the app

How to launch OpenDAM from a checkout, find its data, inspect its databases and screenshot it.
This machine runs Arch with KDE Plasma on Wayland and an NVIDIA GPU.

## Launch

From the repo root:

```sh
make dev
```

- The first run in a fresh worktree compiles about 660 crates, which takes several minutes. The
  app is up when the log prints ``Running `target/debug/opendam-desktop` `` and then
  `NAS-efficient indexing complete`.
- Export nothing. Before Tauri starts, `main.rs` picks the WebKitGTK rendering mode
  (`render::choose_render_env`):
  - If you set any of `WEBKIT_DISABLE_DMABUF_RENDERER`, `WEBKIT_DMABUF_RENDERER_FORCE_SHM`,
    `__NV_DISABLE_EXPLICIT_SYNC`, `WEBKIT_DISABLE_COMPOSITING_MODE` or
    `WEBKIT_SKIA_ENABLE_CPU_RENDERING`, whatever its value, it sets nothing (`user-set`).
  - Otherwise, with the NVIDIA driver loaded (`/proc/driver/nvidia/version`), it sets
    `WEBKIT_DMABUF_RENDERER_FORCE_SHM=1` (`gpu-shm`): GPU painting, compositing layers and threaded
    scrolling, with frames sent through shared memory. WebKit's default DMA-BUF path, and
    `WEBKIT_DISABLE_COMPOSITING_MODE=1`, crash there with `Error 71 (Protocol error) dispatching
    to Wayland display`.
  - Otherwise it sets nothing and WebKit keeps its default (`webkit-default`).

  To override it, export a variable yourself: `WEBKIT_DISABLE_DMABUF_RENDERER=1` gives the old
  CPU rendering, and `__NV_DISABLE_EXPLICIT_SYNC=1` the zero-copy DMA-BUF path. At startup the
  app logs the choice and the resulting mode under the `render` target, for example
  `choice=gpu-shm (...) mode="gpu-shm"`; `render::describe_render_env` names the mode from the
  variables that are set.
- Vite listens on port 1420, which every worktree's `make dev` shares. If another worktree holds
  it, run on 1430 from `apps/desktop`:

  ```sh
  bun run dev --config '{"build":{"devUrl":"http://localhost:1430","beforeDevCommand":"bun run vite:dev --port 1430"}}'
  ```

- Stop only the app you started. Other worktrees run their own `opendam-desktop`, so check a
  process's cwd (`readlink /proc/<pid>/cwd`) before killing it.

## A throwaway profile

To test anything that writes (auto-tagging runs, tag edits), use a throwaway profile, never the
real data:

1. **Fresh data:** point `XDG_DATA_HOME` at a new temp dir, so the master DB, settings and runs
   start empty.
2. **Scratch library:** copy some assets into a temp folder, then register it by seeding the
   throwaway master DB before launch (`add_library` only takes folders picked in the dialog). Run
   `make dev` once with that `XDG_DATA_HOME` so the app creates the master DB, quit it, then:

   ```sh
   export XDG_DATA_HOME=$(mktemp -d)
   SCRATCH=$(mktemp -d)
   cp ~/Documents/Kenney/Assets/<some pack>/*.png "$SCRATCH"/
   # ...run make dev once with this XDG_DATA_HOME, quit, then:
   sqlite3 "$XDG_DATA_HOME/com.opendam.desktop/opendam.sqlite" \
     "INSERT INTO libraries (id, name, root_paths, created_at) VALUES ('scratch', 'Scratch', '[\"$SCRATCH\"]', 0);"
   ```

   The next launch opens and indexes it.
3. **Auto-tagging without a key:** debug builds read these variables. Release builds ignore them.
   - `OPENDAM_AUTOTAG_FAKE=1` adds two tagging providers that tag from file-name words: **Fake**
     (all three modes) and **Fake text-only** (Existing only, like Jev). A file named
     `rock_large.png` gets a catalog tag named "Rock", and in Prefer existing it proposes
     "Large" for review. In Create freely it creates "Large" and applies it.
   - `OPENDAM_AUTOTAG_FAKE_DELAY_MS` (default 300) slows each fake call, so a run can be watched,
     paused or photographed mid-way.
   - `OPENDAM_NO_KEYRING=1` ignores the OS keyring. With `ANTHROPIC_API_KEY` and
     `TYPESAFE_API_KEY` unset too, the app shows the unconfigured state even on a machine with
     saved keys.
   - `OPENDAM_ANTHROPIC_BASE_URL=http://127.0.0.1:<port>` sends Anthropic requests to a local
     stand-in instead of `https://api.anthropic.com`, so the real provider can be exercised and
     photographed with a dummy key and no spend. It must answer `POST /v1/messages` like the API,
     for example with a file from `src-tauri/src/autotag/fixtures/anthropic/`.
   - `OPENDAM_JEV_BASE_URL=http://127.0.0.1:<port>` does the same for Jev instead of
     `https://api.typesafe.ai`. The stand-in answers `POST /v1/systemone`, for example with
     `src-tauri/src/autotag/fixtures/jev/scores.json`, and `TYPESAFE_API_KEY` can be any dummy
     value.
   - `OPENDAM_ANTHROPIC_FIXTURE=<path>` saves the response of a successful Settings "Test
     connection" there, after checking it holds no key. That is how the parser fixture is
     captured with a real key (see that folder's `README.md`).
4. **Claude plan:** set Settings > Anthropic > Access to "Claude plan". Runs then go through the
   `claude` on the app's `PATH`, logged in with your claude.ai account, and count against your
   plan's limits. Each run's vocabulary sits in `$XDG_DATA_HOME/com.opendam.desktop/autotag/<run
   id>/system.txt` while the run can still send requests. A throwaway `XDG_DATA_HOME` doesn't
   change which login `claude` uses: that lives in `~/.claude`.

```sh
XDG_DATA_HOME=$XDG_DATA_HOME OPENDAM_AUTOTAG_FAKE=1 OPENDAM_AUTOTAG_FAKE_DELAY_MS=1500 \
  OPENDAM_NO_KEYRING=1 make dev
```

Runs live in the master DB's `tag_runs` and `tag_run_items` tables, and every tag a run adds is
recorded in `tag_run_applied` before the library sees it. A Prefer existing run's new tag names
wait for review in `tag_run_proposals` and `tag_run_proposal_assets`. A Create freely run records
each tag it created there too, as an `accepted` proposal with `created_tag = 1`. The desktop notification a
run sends needs a notification server, which the nested headless KWin used for screenshots lacks.

## Checking the CSP

`make dev` loads the page from Vite's `devUrl`, and Tauri only applies `app.security.csp` to the
frontend it embeds, so dev mode runs with no CSP. To see the policy in force with devtools still
available, build the embedded frontend in debug mode from `apps/desktop` and run the binary:

```sh
bun run build --debug --no-bundle
../../scripts/memory-cap.sh ./src-tauri/target/debug/opendam-desktop
```

## Where the data lives

- **Master DB:** `~/.local/share/com.opendam.desktop/opendam.sqlite` (the Tauri app data dir,
  `$XDG_DATA_HOME/com.opendam.desktop/`). It holds the `libraries` table and a copy of every tag
  and collection definition. The webview's storage sits in the same directory.
- **Library DB:** `<library root>/.opendam/library.sqlite`, where the root is the first entry of
  `libraries.root_paths`. Thumbnails go in `<library root>/.opendam/thumbs/`.
- The registered library is `~/Documents/Kenney/Assets` (about 83k assets).

`make dev` uses this real data. Treat it as read-only: never rename, move or delete assets, tags or
collections to test a change.

## Inspecting the databases

The running app holds locks on both DBs. Open them read-only with a busy timeout so a query waits
for the lock instead of failing with `database is locked`:

```sh
sqlite3 -readonly -cmd ".timeout 20000" ~/.local/share/com.opendam.desktop/opendam.sqlite "SELECT id, name, root_paths, asset_count FROM libraries;"
sqlite3 -readonly -cmd ".timeout 20000" ~/Documents/Kenney/Assets/.opendam/library.sqlite "SELECT COUNT(*) FROM assets;"
```

## Screenshots for workflow evidence

Capture the app window into `.evidence/`, which is gitignored:

```sh
mkdir -p .evidence
spectacle -b -n -a -o .evidence/app-window.png
```

`-a` captures the active window, so the app must have focus. A freshly launched window does. If
something else took focus since, raise the app first: KDE doesn't raise a window on request, so
bring it forward by hand or with a KWin script (`qdbus6 org.kde.KWin /Scripting loadScript ...`
setting `workspace.activeWindow`). Look at the PNG before submitting it as evidence.

## Scroll benchmark

A dev-only harness, `apps/desktop/src/dev/scrollBench.ts`, measures how the asset grid scrolls on
the Kenney bench data, so every change to the grid is judged on the same numbers
([plan](../plans/fast-collection-browsing/plan.md), "Scroll benchmark"). `main.tsx` loads it only
when `import.meta.env.DEV` is true and `VITE_SCROLL_BENCH` is set, so release builds don't contain
it. Its Tauri commands (`bench_rss`, `bench_render_mode`, `bench_log`, `bench_only`,
`bench_report`) exist only under `debug_assertions`.

- `VITE_SCROLL_BENCH=1` runs the timing scenarios at sm, md and lg: continuous scroll (60 s at
  3,000 px/s), end to end, and 20 seeded random jumps. On the grid that appends pages,
  end to end jumps to the bottom until 20,000 tiles have loaded or 120 s pass, and random jumps
  are reported as unsupported. Started by hand (`VITE_SCROLL_BENCH=1 make dev`), it runs all of
  them in one process, so its RSS figures carry over between scenarios; use the script for
  numbers to compare.
- `VITE_SCROLL_BENCH=checks` runs the UI checks registered in `src/dev/uiChecks.ts` and writes
  PASS or FAIL for each. A check calls `pause(name)` where a screenshot should be taken; the
  harness always pauses once at the start, as `checks-start`.

The UI checks, each defined in its plan:

- **U1-U8** (grid windowing, scrub label, keyboard, image scaling):
  [fast-collection-browsing](../plans/fast-collection-browsing/plan.md), "UI checks".
- **M1**: the web process grows under 200 MB in 10 s held still deep in the grid (AGENTS.md, "No
  `backdrop-filter` in the grid or the list").
- **U9** (folder tree): the bench library's row expands into its top-level folders, each showing
  its `get_folders` item count. Selecting `3D assets/Platformer Kit` gives a grid total equal to
  its node's count, every result lies below it, and the status bar shows its path. Then, on a
  folder holding one file, a rename keeps the count, and deleting the file's rows (not the file)
  drops the folder from the tree and takes one from every folder above it, with no manual
  refresh. It also checks that every `get_folders` call on the way took under 50 ms. On a fresh
  run copy no write commits while the thumbnail cleanup's long read runs, so U9 retries the
  rename, which a failed write undoes, until one lands.
  [smarter-big-collections](../plans/smarter-big-collections/plan.md), step 1. Screenshots:
  `U9-top-level-folders`, `U9-platformer-kit`, `U9-after-delete`.
- **U10** (packs and hidden files): `3D assets/Car Kit` is marked as a pack in the tree, a pack
  in `Audio/` whose files are all hidden still shows with a count of 0, and the kit's grid holds
  neither `License.txt` nor `Preview.png`, while the status bar reads `<items> items (<files>
  files)` with the kit's hidden files counted as files only. A search for "license" in the kit
  then reads `0 items (1 file)`, and "Show hidden files" brings `License.txt` back and makes the
  kit's badge count its hidden files.
  [smarter-big-collections](../plans/smarter-big-collections/plan.md), step 2. Screenshots:
  `U10-license-hidden`, `U10-license-shown`.
- **U11** (format groups): in `3D assets/Car Kit`, the `GLB format/`, `FBX format/` and `OBJ
  format/` nodes each count as many items as `Models/`, and selecting `Models/` lists each model
  once, as its GLB, with a solid `GLB +2` badge whose tooltip reads `3 files: GLB, FBX, OBJ`, on
  its card and, in the list view, unclipped in its row's extension column. Each card of a model
  with a Kenney render shows that render, and the kit lists no `Previews/` file.
  Selecting `FBX format/` lists the same items and no FBX file, and adding `.glb` to it matches
  nothing. Filtering the kit by FBX lists every model, each as its GLB, and a search for "fbx"
  finds `sedan.glb` through its FBX copy's folder.
  [smarter-big-collections](../plans/smarter-big-collections/plan.md), step 3. Screenshots:
  `U11-kit-models`, `U11-list-badge`, `U11-fbx-format`.
- **U12** (group detail): selecting `sedan.glb` in `3D assets/Car Kit/Models` lists its GLB, FBX
  and OBJ files in the detail panel, in that order, each with its format, folder and size. Each
  file's Open, Reveal and Copy path, recorded rather than run, act on that file, in its row and in
  its own details, which show on a click with "Back to group". Moving between the FBX and OBJ
  files and back leaves the grid's selection and cached pages unchanged. Selecting the kit's
  `colormap.png` then lists its copies, each row showing that copy's own image.
  [smarter-big-collections](../plans/smarter-big-collections/plan.md), C19. Screenshots:
  `U12-group-files`, `U12-member`, `U12-2d-members`.

Run it with the script, from the repo root:

```sh
apps/desktop/scripts/scroll-bench.sh          # timing scenarios, about 10 minutes plus the copy
apps/desktop/scripts/scroll-bench.sh checks   # UI checks
BENCH_CHECKS=U8 apps/desktop/scripts/scroll-bench.sh checks   # only U8, in a fresh app process
```

The script:

1. copies the golden snapshot (below) to a fresh `~/.cache/opendam-bench/run-*` directory and
   starts the app with a fresh `XDG_DATA_HOME` and `OPENDAM_BENCH_LIBRARY` set to the copy. Debug
   builds read that variable at startup, register the copy as library `bench` in the master DB
   they just created, so the app's own schema is used, and open it;
2. builds a debug binary with the frontend embedded and built in Vite's development mode
   (`NODE_ENV=development vite build --mode development`), with `VITE_SCROLL_BENCH` set;
3. runs the app in a nested headless KWin with a virtual 1400x900 output, its own D-Bus session
   and `OPENDAM_NO_KEYRING=1`, never on your desktop session. First a warm-up process runs the
   one-time thumbnail cleanup pass to the end, until the copy's `library_meta` records
   `thumbs_no_upscale = done` (about 25 minutes on Kenney), and its report is discarded. Each
   process would otherwise start the pass over, so every scenario would share the machine with
   it and measure thumbnails it is about to clear. A timing run starts a fresh app
   process for each size and scenario, naming it in `OPENDAM_SCROLL_BENCH_ONLY` (for example
   `md/endToEnd`), because memory one scenario takes stays with the web process and would count
   toward the next one's RSS. It then merges the nine reports into one. Checks run in one process;
4. screenshots each `[scroll-bench] pause <name>` line the app prints, with `spectacle`, into
   `.evidence/scroll-bench/<name>.png`, and stops the app at `[scroll-bench] done`;
5. deletes the run directory.

The app runs under `scripts/memory-cap.sh`, in its own systemd scope capped at
`OPENDAM_MEMORY_MAX` (default `10G`), so a web process that runs away is killed alone; the cargo
build before it is not capped. In `checks` mode one web process reached about 20 GB before the
cap existed. When the cap kills the app, the script says so, prints the last `[scroll-bench]`
lines and exits 137: report that as a finding, don't rerun past it. Never start a run while
another worktree's is going (`pgrep -f '^bash .*scroll-bench\.sh'`). See AGENTS.md,
"Memory-heavy tools".

It doesn't use the Vite dev server: under its `http://localhost` origin, every `<img>` loaded
from the asset protocol fails in WebKitGTK 2.54 (libpng logs `Read Error`, while `fetch` of the
same URL returns the whole file), so the grid would show no images. The embedded frontend loads
them. This was seen in the nested KWin; `make dev` on your own session is unverified.

The app picks its usual rendering mode (`gpu-shm` on this machine). To bench another, export its
variable for the script, for example `WEBKIT_DISABLE_DMABUF_RENDERER=1` for `cpu`; the app
inherits it and `bench_render_mode` reports it.
`BENCH_TIMEOUT` (seconds, default 1800) stops a stuck app launch, and the warm-up. While the
cleanup pass clears rows it holds the library's connections, for about 16 minutes on Kenney after
about 7 minutes of reading image headers: an app started then shows an empty grid until the pass
ends with `library:updated`, which is why the harness waits up to 20 minutes for the first page.

**Reports** land in `apps/desktop/src-tauri/target/scroll-bench/` (gitignored), as
`<render mode>-<unix ts>.json` for timing runs and `<render mode>-checks-<unix ts>.json` for
checks. The app also prints each report after `[scroll-bench] report <path>`, and the script
copies the last run's app log to `last-run.log` there. A report holds, per grid size and scenario,
frame intervals (p50, p95, max, counts over 50 and 100 ms), DOM node min and max, web-process RSS
(start, end, max), jump time-to-ready, and the run context: render mode, window size,
`devicePixelRatio`, total assets, whether the grid is windowed, and the git commit.
The CPU-mode baseline of the grid before windowing is
[`docs/plans/fast-collection-browsing/baseline-cpu.json`](../plans/fast-collection-browsing/baseline-cpu.json).

**DOM markers** the harness reads: the grid's scroll container is `#asset-scroll`, and every
tile carries `data-tile`, with `data-tile-state="ready"` once its asset is loaded and its image
has loaded or failed, `"placeholder"` before that. The windowed grid also gives every tile
`data-index` (its position in the results) and a tile whose page isn't cached yet
`data-placeholder`; `#asset-scroll` carries the layout as `data-columns` and `data-row-pitch`,
and the footer's visible range is `#asset-range`.

### Bench data

Builds from the fast-collection-browsing branches never open `~/Documents/Kenney/Assets` or the
real master DB, because they write to any library they open. They run on copies of a golden
snapshot, made once:

```sh
G=~/.cache/opendam-bench/kenney-golden L=~/Documents/Kenney/Assets
rsync -a --exclude='/.opendam/library.sqlite*' --exclude='/.opendam/index.lock' \
  --exclude='/.opendam/backup-*' "$L/" "$G/"
sqlite3 -readonly "$L/.opendam/library.sqlite" ".backup '$G/.opendam/library.sqlite'"
```

`.backup` stays consistent even while your app holds the DB. `~/.cache/opendam-bench/golden.json`
records the source, the time, the asset count (83,095), the size (about 5.3 GB) and the commit.
Every comparison uses this same snapshot, so a baseline can be rerun on identical data at any
later step; your live library only gets new indexes or cleanup passes when you run a build that
has them. Each run copy takes as much disk again until the script deletes it. Don't refresh the
snapshot without rerunning the baseline: comparisons need identical data.

