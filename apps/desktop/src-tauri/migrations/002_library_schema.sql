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
