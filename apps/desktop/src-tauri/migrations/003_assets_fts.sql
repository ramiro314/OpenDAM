-- OpenDAM per-library full-text search (FTS5), run after 002_library_schema.sql.
--
-- `assets_fts` indexes the external content of `assets`. `file_path` makes search find an asset
-- by the folder names in its path. Every 'delete' row must supply exactly the values that were
-- indexed, or the index is silently corrupted, so all three triggers write every column.

CREATE VIRTUAL TABLE IF NOT EXISTS assets_fts USING fts5(
    id UNINDEXED,
    file_name,
    description,
    metadata,
    file_path,
    content='assets',
    content_rowid='rowid'
);

CREATE TRIGGER IF NOT EXISTS assets_ai AFTER INSERT ON assets BEGIN
    INSERT INTO assets_fts(rowid, id, file_name, description, metadata, file_path)
    VALUES (new.rowid, new.id, new.file_name, new.description, new.metadata, new.file_path);
END;

CREATE TRIGGER IF NOT EXISTS assets_ad AFTER DELETE ON assets BEGIN
    INSERT INTO assets_fts(assets_fts, rowid, id, file_name, description, metadata, file_path)
    VALUES ('delete', old.rowid, old.id, old.file_name, old.description, old.metadata, old.file_path);
END;

-- Fires only when an indexed column changes, so writes to other asset columns never touch the
-- index. A rename changes `file_path`, so it still reindexes.
CREATE TRIGGER IF NOT EXISTS assets_au AFTER UPDATE OF file_name, description, metadata, file_path
ON assets BEGIN
    INSERT INTO assets_fts(assets_fts, rowid, id, file_name, description, metadata, file_path)
    VALUES ('delete', old.rowid, old.id, old.file_name, old.description, old.metadata, old.file_path);
    INSERT INTO assets_fts(rowid, id, file_name, description, metadata, file_path)
    VALUES (new.rowid, new.id, new.file_name, new.description, new.metadata, new.file_path);
END;
