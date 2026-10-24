-- OpenDAM Per-Library SQLite Schema (stored in <library_root>/.opendam/library.sqlite)

PRAGMA journal_mode=DELETE;
PRAGMA synchronous=NORMAL;
PRAGMA temp_store=MEMORY;
PRAGMA cache_size=-64000;
PRAGMA busy_timeout=30000;  -- 30s for slow NAS writes
PRAGMA foreign_keys=OFF;

-- ── Directory modification timestamps for smart incremental scans ────────
CREATE TABLE IF NOT EXISTS dir_mtimes (
    dir_path  TEXT PRIMARY KEY,
    mtime     INTEGER NOT NULL
);

-- ── Tags ───────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS tags (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    color      TEXT,            -- color label enum string
    parent_id  TEXT REFERENCES tags(id) ON DELETE SET NULL,
    description TEXT NOT NULL DEFAULT '' -- hint sent to the tagging providers
);

-- ── Assets ─────────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS assets (
    id               TEXT PRIMARY KEY,
    file_path        TEXT NOT NULL UNIQUE, -- relative to library root
    file_name        TEXT NOT NULL,
    extension        TEXT NOT NULL,
    kind             TEXT NOT NULL,
    size_bytes       INTEGER NOT NULL,
    modified_at      INTEGER NOT NULL,
    indexed_at       INTEGER NOT NULL,
    rating           INTEGER NOT NULL DEFAULT 0,
    color_label      TEXT NOT NULL DEFAULT 'none',
    is_favorite      INTEGER NOT NULL DEFAULT 0,
    description      TEXT NOT NULL DEFAULT '',
    thumbnail_path   TEXT,                -- relative to library root (.opendam/thumbs/...)
    preview_status   TEXT NOT NULL DEFAULT 'none',
    metadata         TEXT NOT NULL DEFAULT '{}', -- JSON object
    support_role     TEXT,                -- 'doc' or 'cover' or 'model_preview' for a support file (structure.rs)
    group_id         TEXT,                -- its format group's main item (itself included) (structure.rs)
    bundled_preview  TEXT,                -- the render a main item shows (relative to library root) (structure.rs)
    render_width     INTEGER,             -- a render's size from its header, NULL until read (structure.rs)
    render_height    INTEGER,
    render_read_bytes INTEGER,            -- the size_bytes and modified_at the header was read at
    render_read_mtime INTEGER,
    preview_version  INTEGER,             -- the preview engine version that made the preview, NULL for 1 (preview.rs)
    preview_textures TEXT                 -- the engine's texture report as JSON, never searched (preview.rs)
);

CREATE INDEX IF NOT EXISTS idx_assets_kind         ON assets(kind);
CREATE INDEX IF NOT EXISTS idx_assets_rating       ON assets(rating);
CREATE INDEX IF NOT EXISTS idx_assets_color_label  ON assets(color_label);
CREATE INDEX IF NOT EXISTS idx_assets_is_favorite  ON assets(is_favorite);
CREATE INDEX IF NOT EXISTS idx_assets_modified_at  ON assets(modified_at);
CREATE INDEX IF NOT EXISTS idx_assets_extension    ON assets(extension);
CREATE INDEX IF NOT EXISTS idx_assets_file_name_nocase ON assets(file_name COLLATE NOCASE);
CREATE INDEX IF NOT EXISTS idx_assets_size_bytes   ON assets(size_bytes);

-- ── Asset ↔ Tag join table ──────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS asset_tags (
    asset_id  TEXT NOT NULL REFERENCES assets(id)  ON DELETE CASCADE,
    tag_id    TEXT NOT NULL REFERENCES tags(id)    ON DELETE CASCADE,
    PRIMARY KEY (asset_id, tag_id)
);

-- ── Collections ────────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS collections (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    description  TEXT NOT NULL DEFAULT '',
    parent_id    TEXT REFERENCES collections(id) ON DELETE CASCADE,
    is_smart     INTEGER NOT NULL DEFAULT 0,
    filter_spec  TEXT,
    created_at   INTEGER NOT NULL
);

-- ── Collection ↔ Asset join table ─────────────────────────────────────────
CREATE TABLE IF NOT EXISTS collection_assets (
    collection_id  TEXT NOT NULL REFERENCES collections(id) ON DELETE CASCADE,
    asset_id       TEXT NOT NULL REFERENCES assets(id)      ON DELETE CASCADE,
    PRIMARY KEY (collection_id, asset_id)
);

-- ── Preview jobs ──────────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS preview_jobs (
    id            TEXT PRIMARY KEY,
    asset_id      TEXT NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    status        TEXT NOT NULL DEFAULT 'queued',
    progress      INTEGER,
    error_message TEXT,
    created_at    INTEGER NOT NULL
);

-- ── Folders: derived by the structure pass (structure.rs), never edited ──────
-- Every folder with at least one indexed file below it, and the root (`path = ''`, no parent).
-- `item_count` counts the grid items with a visible file below it, recursively (a format group
-- once), and `hidden_count` the support files. `split_groups` counts the groups with a member
-- below it whose main item lies outside it. `pack_json` stays NULL until pack info is parsed.
CREATE TABLE IF NOT EXISTS folders (
    path          TEXT PRIMARY KEY,   -- relative to library root, no edge slashes
    parent_path   TEXT,               -- NULL for the root
    name          TEXT NOT NULL,
    is_pack       INTEGER NOT NULL DEFAULT 0,
    item_count    INTEGER NOT NULL DEFAULT 0,
    hidden_count  INTEGER NOT NULL DEFAULT 0,
    pack_json     TEXT,
    split_groups  INTEGER NOT NULL DEFAULT 0
);

-- `get_folders` reads one node's children in name order from this index alone.
CREATE INDEX IF NOT EXISTS idx_folders_parent_name ON folders(parent_path, name COLLATE NOCASE);

-- ── Pack overrides: your edits to packs, which the structure pass reads and never writes ──────
-- Keyed by the folder's relative path, so an edit outlives the folder going away and coming
-- back, as an update extracted over the library does. `mark` is 'pack' ("Mark as pack") or
-- 'not_pack' ("Not a pack"), and NULL leaves it to the rules. NULL fields keep what the pass
-- derives. `cover_path` is relative to the library root.
CREATE TABLE IF NOT EXISTS pack_overrides (
    path        TEXT PRIMARY KEY,
    mark        TEXT CHECK (mark IN ('pack', 'not_pack')),
    name        TEXT,
    license     TEXT,
    source_url  TEXT,
    cover_path  TEXT,
    updated_at  INTEGER NOT NULL,
    -- The game export's pack origin and normal-map convention (plan game-export C6)
    origin             TEXT CHECK (origin IN ('unity', 'unreal', 'generic')),
    normal_convention  TEXT CHECK (normal_convention IN ('opengl', 'directx'))
);

-- ── File hashes: source sha256s memoized for the game export (export/hashes.rs) ──────
-- A row holds while the file keeps the size and mtime (nanoseconds) it was hashed at.
CREATE TABLE IF NOT EXISTS file_hashes (
    file_path    TEXT PRIMARY KEY,  -- relative to library root
    size_bytes   INTEGER NOT NULL,
    modified_at  INTEGER NOT NULL,
    sha256       TEXT NOT NULL
);

-- ── Export edits: your game export corrections (plan game-export C4, export/edits.rs) ──────
-- Only what you set (D32), as a JSON object, on every member of the asset's group. Kept out of
-- `metadata`, which `assets_fts` indexes.
CREATE TABLE IF NOT EXISTS export_edits (
    asset_id  TEXT PRIMARY KEY REFERENCES assets(id) ON DELETE CASCADE,
    edits     TEXT NOT NULL
);

-- The full-text table and its triggers are in 003_assets_fts.sql, so a library made before
-- `file_path` was indexed can recreate them inside a transaction (db::index_file_paths).

-- ── Library meta: one-time maintenance passes record themselves here ──────
CREATE TABLE IF NOT EXISTS library_meta (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);

-- ── Pack imports: packs built from their originals (docs/plans/pack-import/plan.md, Records) ──
-- An original lives outside every library, so `original_path` is absolute: the one exception to
-- relative paths in a library DB. Every other path here is relative to the library root.
-- `original_state` is what the last check of the original found (import/original.rs): 'ok',
-- 'missing', or 'changed', a file at `original_path` whose sha256 differs. That file's stamp and
-- sha256 are kept in `seen_*`, so it is hashed again only once it changes again.
CREATE TABLE IF NOT EXISTS imports (
    id                 TEXT PRIMARY KEY,
    original_path      TEXT NOT NULL,
    original_size      INTEGER NOT NULL,
    original_mtime_ns  INTEGER NOT NULL,
    original_sha256    TEXT NOT NULL,
    kind               TEXT NOT NULL CHECK (kind IN ('unitypackage', 'zip')),
    created_at         INTEGER NOT NULL,
    original_state     TEXT NOT NULL DEFAULT 'ok'
                       CHECK (original_state IN ('ok', 'missing', 'changed')),
    seen_size          INTEGER,
    seen_mtime_ns      INTEGER,
    seen_sha256        TEXT
);

-- One pack of an import. `current_build` names the committed build (its `import_outputs` rows are
-- the pack's manifest), NULL before the first. `recipe_rev` grows with every change to its steps.
-- `swap_state` journals a swap in progress: 'swapping' with `swap_build` and `swap_prev`, whether
-- the swap moved a live folder aside, or 'removing'. `store_page` keeps the Kenney store page
-- lookup's answer outside the recipe: JSON {"urls", "page"}, the URLs tried and the page found
-- (null for none), NULL before an answer.
CREATE TABLE IF NOT EXISTS import_packs (
    id             TEXT PRIMARY KEY,
    import_id      TEXT NOT NULL REFERENCES imports(id) ON DELETE CASCADE,
    source         TEXT NOT NULL DEFAULT '',
    target         TEXT NOT NULL UNIQUE,
    preset         TEXT NOT NULL,
    built_at       INTEGER,
    status         TEXT NOT NULL DEFAULT 'never_built'
                   CHECK (status IN ('built', 'failed', 'never_built')),
    current_build  TEXT,
    recipe_rev     INTEGER NOT NULL DEFAULT 0,
    store_page     TEXT,
    swap_state     TEXT CHECK (swap_state IN ('swapping', 'removing')),
    swap_build     TEXT,
    swap_prev      INTEGER
);

CREATE TABLE IF NOT EXISTS import_steps (
    pack_id   TEXT NOT NULL REFERENCES import_packs(id) ON DELETE CASCADE,
    position  INTEGER NOT NULL,
    kind      TEXT NOT NULL,
    enabled   INTEGER NOT NULL DEFAULT 1,
    options   TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (pack_id, position)
);

-- Every file a build wrote into its pack. `source_path` is a JSON array, NULL for a kept file
-- added outside OpenDAM. The file key, `source_path` and whether the file is generated, is unique
-- within a build.
CREATE TABLE IF NOT EXISTS import_outputs (
    pack_id      TEXT NOT NULL,
    build_id     TEXT NOT NULL,
    path         TEXT NOT NULL,
    size         INTEGER NOT NULL,
    mtime_ns     INTEGER NOT NULL,
    sha256       TEXT NOT NULL,
    origin       TEXT NOT NULL CHECK (origin IN ('extracted', 'generated', 'kept')),
    source_path  TEXT,
    PRIMARY KEY (pack_id, build_id, path)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_import_outputs_file_key
    ON import_outputs(pack_id, build_id, source_path, origin = 'generated')
    WHERE source_path IS NOT NULL;

-- The asset rows a build carried to new paths ('moved') and the rows its index inserted
-- ('inserted', `from_path` NULL), so a rollback can undo them. Deleted once the build settles.
CREATE TABLE IF NOT EXISTS import_moves (
    pack_id    TEXT NOT NULL,
    build_id   TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('moved', 'inserted')),
    asset_id   TEXT NOT NULL,
    from_path  TEXT,
    to_path    TEXT NOT NULL,
    PRIMARY KEY (pack_id, build_id, seq)
);

-- Import runs, a durable queue (import/engine.rs). `packs` is a JSON array of
-- {"packId", "recipeRev"}, each pack's `recipe_rev` when it was queued, and `warnings` a JSON
-- array of the entries a build skipped. A run belongs to the process that queued it: its owner
-- (its machine and pid namespace, its boot, pid and start time, `watcher::this_process`) and a
-- heartbeat its task rewrites.
CREATE TABLE IF NOT EXISTS import_runs (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL CHECK (kind IN ('import', 'rebuild', 'remove')),
    packs           TEXT NOT NULL,
    drift           TEXT NOT NULL DEFAULT '{}',
    status          TEXT NOT NULL CHECK (status IN
                    ('queued', 'running', 'done', 'failed', 'cancelled', 'interrupted')),
    phase           TEXT,
    progress_done   INTEGER NOT NULL DEFAULT 0,
    progress_total  INTEGER NOT NULL DEFAULT 0,
    error           TEXT,
    warnings        TEXT NOT NULL DEFAULT '[]',
    created_at      INTEGER NOT NULL,
    started_at      INTEGER,
    finished_at     INTEGER,
    owner_machine   TEXT,
    owner_boot      TEXT,
    owner_pid       INTEGER NOT NULL,
    owner_started   INTEGER NOT NULL,
    heartbeat_at    INTEGER
);
CREATE INDEX IF NOT EXISTS idx_import_runs_status ON import_runs(status, created_at);
