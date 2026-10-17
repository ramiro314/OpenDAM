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
- Export nothing. `main.rs` sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` when it is unset; without it
  the window crashes with `Error 71 (Protocol error) dispatching to Wayland display`.
- Vite listens on port 1420, which every worktree's `make dev` shares. If another worktree holds
  it, run on 1430 from `apps/desktop`:

  ```sh
  bun run tauri dev --config '{"build":{"devUrl":"http://localhost:1430","beforeDevCommand":"bun run vite:dev --port 1430"}}'
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
bun run tauri build --debug --no-bundle
./src-tauri/target/debug/opendam-desktop
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
