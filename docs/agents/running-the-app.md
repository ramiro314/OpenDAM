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
