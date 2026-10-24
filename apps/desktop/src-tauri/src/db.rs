use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Pool, Sqlite};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct LibraryHandle {
    pub id: String,
    pub name: String,
    pub root_path: PathBuf,
    pub pool: Pool<Sqlite>,
}

/// The master DB is the tag and collection catalog. Each library DB keeps a mirror of those
/// definitions plus its own `asset_tags` and `collection_assets` rows, for its own assets only.
pub struct Database {
    pub master_pool: Pool<Sqlite>,
    pub libraries: Arc<RwLock<HashMap<String, LibraryHandle>>>,
    /// One lock per library id, held by an index run and by each structure pass (`index_lock`).
    index_locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Held by every tag or collection definition update and delete, and by each batch of a bulk
    /// folder write (`commands/folder.rs`) from reading the definition until its commit, so a
    /// batch never copies a definition into a mirror while it is changed or removed.
    pub catalog_lock: tokio::sync::Mutex<()>,
    /// Names each library whose late structure pass a command's follow-up task ran
    /// (`structure::rebuild_library`), so the app can emit `library:updated`.
    pub late_passes: tokio::sync::broadcast::Sender<String>,
    /// The libraries with a follow-up task waiting to run their late structure pass, at most one
    /// each (`structure::rebuild_library`).
    pub follow_ups: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

/// A collection definition as stored: id, name, description, parent_id, is_smart, filter_spec,
/// created_at.
pub type CollectionRow = (
    String,
    String,
    String,
    Option<String>,
    bool,
    Option<String>,
    i64,
);

/// A tag definition as stored: id, name, color, parent_id, description (the hint).
pub type TagRow = (String, String, Option<String>, Option<String>, String);

/// Inserts a new tag definition into the master catalog, on the caller's connection or inside
/// its transaction. Every way a tag is created goes through here: the create-tag command, a
/// review's apply and a Create freely run. Callers check the name and the parent first.
pub async fn create_master_tag(
    conn: &mut sqlx::SqliteConnection,
    id: &str,
    name: &str,
    parent_id: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO tags (id, name, parent_id) VALUES (?, ?, ?)")
        .bind(id)
        .bind(name)
        .bind(parent_id)
        .execute(conn)
        .await
        .map(|_| ())
}

/// Writes a tag definition to a mirror, updating an existing row in place. Never `REPLACE`:
/// it deletes the old row first, and `ON DELETE CASCADE` would take the library's memberships.
pub async fn upsert_tag<'e, E: sqlx::Executor<'e, Database = Sqlite>>(
    executor: E,
    (id, name, color, parent_id, description): &TagRow,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO tags (id, name, color, parent_id, description) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             name = excluded.name, color = excluded.color, parent_id = excluded.parent_id,
             description = excluded.description",
    )
    .bind(id)
    .bind(name)
    .bind(color)
    .bind(parent_id)
    .bind(description)
    .execute(executor)
    .await
    .map(|_| ())
}

/// Writes a collection definition to a mirror, updating an existing row in place (see
/// `upsert_tag` for why it is never `REPLACE`).
pub async fn upsert_collection<'e, E: sqlx::Executor<'e, Database = Sqlite>>(
    executor: E,
    (id, name, description, parent_id, is_smart, filter_spec, created_at): &CollectionRow,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO collections (id, name, description, parent_id, is_smart, filter_spec, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             name = excluded.name, description = excluded.description,
             parent_id = excluded.parent_id, is_smart = excluded.is_smart,
             filter_spec = excluded.filter_spec",
    )
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(parent_id)
    .bind(is_smart)
    .bind(filter_spec)
    .bind(created_at)
    .execute(executor)
    .await
    .map(|_| ())
}

/// The per-library schema. The full-text table and its triggers are a separate script so
/// `index_file_paths` can recreate them inside a transaction, which the schema's pragmas forbid.
const LIBRARY_SCHEMA: &str = include_str!("../migrations/002_library_schema.sql");
const ASSETS_FTS_SCHEMA: &str = include_str!("../migrations/003_assets_fts.sql");

/// Creates whatever the library schema lacks on `pool`, the full-text index included.
async fn create_library_schema(pool: &Pool<Sqlite>) -> sqlx::Result<()> {
    sqlx::query(LIBRARY_SCHEMA).execute(pool).await?;
    sqlx::query(ASSETS_FTS_SCHEMA).execute(pool).await?;
    Ok(())
}

/// The `library_meta` key recording that `index_file_paths` converted a library.
const FTS_FILE_PATH_KEY: &str = "fts_file_path";

/// Backward compatibility: `assets_fts` gained `file_path`, so search finds an asset by the
/// folder names in its path. FTS5 can't add a column, and `IF NOT EXISTS` keeps an old table and
/// its triggers, so a library whose index lacks the column drops the table and its three
/// triggers, recreates them and rebuilds the index from `assets`. That is one transaction: a
/// conversion interrupted before its commit leaves the old index, and the next open runs it
/// again. A converted library has the column, so it is never converted twice, and
/// `library_meta` records the conversion. Returns whether it converted. `stop_before_commit` is
/// a test hook that rolls the conversion back instead of committing it.
pub(crate) async fn index_file_paths(
    pool: &Pool<Sqlite>,
    stop_before_commit: bool,
) -> sqlx::Result<bool> {
    let indexed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('assets_fts') WHERE name = 'file_path')",
    )
    .fetch_one(pool)
    .await?;
    if indexed {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    sqlx::query(
        "DROP TRIGGER IF EXISTS assets_ai;
         DROP TRIGGER IF EXISTS assets_ad;
         DROP TRIGGER IF EXISTS assets_au;
         DROP TABLE IF EXISTS assets_fts;",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(ASSETS_FTS_SCHEMA).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO assets_fts(assets_fts) VALUES ('rebuild')")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO library_meta (key, value) VALUES (?, 'done')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(FTS_FILE_PATH_KEY)
    .execute(&mut *tx)
    .await?;
    if stop_before_commit {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

/// Backward compatibility: tags gained a description (the hint the tagging providers get).
/// Any failure other than the column already being there is an error: a database left without
/// it fails every tag query later, far from the cause.
async fn add_tag_description_column(pool: &Pool<Sqlite>) -> sqlx::Result<()> {
    let present: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('tags') WHERE name = 'description')",
    )
    .fetch_one(pool)
    .await?;
    if !present {
        sqlx::query("ALTER TABLE tags ADD COLUMN description TEXT NOT NULL DEFAULT ''")
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// Backward compatibility: assets gained `support_role`, `group_id`, `bundled_preview` and a
/// render's cached size (`render_*`), and folders `split_groups`, which the structure pass
/// derives. A library that gains the sizes reads its renders' headers in the pass at open, and
/// keeps the thumbnails it has. As with tag descriptions, any failure other
/// than a column already being there is an error, since every search reads them. Their indexes are
/// made here rather than in the schema, which runs before an old library has the columns.
async fn add_structure_columns(pool: &Pool<Sqlite>) -> sqlx::Result<()> {
    add_missing_columns(
        pool,
        &[
            ("assets", "support_role", "TEXT"),
            ("assets", "group_id", "TEXT"),
            ("assets", "bundled_preview", "TEXT"),
            ("assets", "render_width", "INTEGER"),
            ("assets", "render_height", "INTEGER"),
            ("assets", "render_read_bytes", "INTEGER"),
            ("assets", "render_read_mtime", "INTEGER"),
            ("folders", "split_groups", "INTEGER NOT NULL DEFAULT 0"),
        ],
    )
    .await?;
    // The grid items by path: a search of the library or of a folder counts them from this
    // index alone, and finds a folder's items without reading its members and support files.
    // Its `WHERE` must stay term for term the visibility rule's (`commands::asset`'s
    // `NOT_SUPPORT` and `MAIN_OR_SINGLE`), or SQLite won't use it.
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_assets_group_id ON assets(group_id);
         CREATE INDEX IF NOT EXISTS idx_assets_items ON assets(file_path)
             WHERE support_role IS NULL AND (group_id IS NULL OR group_id = id);",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Backward compatibility: the run tables' `store::ADDED_COLUMNS`. As with tag descriptions, any
/// failure other than a column already being there is an error, since every run query selects
/// them.
async fn add_run_columns(pool: &Pool<Sqlite>) -> sqlx::Result<()> {
    add_missing_columns(pool, &crate::autotag::store::ADDED_COLUMNS).await
}

/// Backward compatibility: assets gained the preview engine version that made a preview and the
/// engine's texture report (plan textured-fbx-previews C8), which `preview::complete_job`
/// stores. Older rows read as `NULL`, version 1. As with tag descriptions, any failure other
/// than a column already being there is an error. Neither is indexed or searched.
async fn add_preview_columns(pool: &Pool<Sqlite>) -> sqlx::Result<()> {
    add_missing_columns(
        pool,
        &[
            ("assets", "preview_version", "INTEGER"),
            ("assets", "preview_textures", "TEXT"),
        ],
    )
    .await
}

/// Backward compatibility: `pack_overrides` gained your pack origin and normal-map convention
/// for the game export (plan game-export C6). Older rows read as `NULL`, inferred. As with tag
/// descriptions, any failure other than a column already being there is an error, since the
/// structure pass reads them.
async fn add_pack_override_columns(pool: &Pool<Sqlite>) -> sqlx::Result<()> {
    add_missing_columns(
        pool,
        &[
            (
                "pack_overrides",
                "origin",
                "TEXT CHECK (origin IN ('unity', 'unreal', 'generic'))",
            ),
            (
                "pack_overrides",
                "normal_convention",
                "TEXT CHECK (normal_convention IN ('opengl', 'directx'))",
            ),
        ],
    )
    .await
}

/// Adds each `(table, column, definition)` of `columns` that its table lacks.
async fn add_missing_columns(
    pool: &Pool<Sqlite>,
    columns: &[(&str, &str, &str)],
) -> sqlx::Result<()> {
    for (table, column, definition) in columns {
        let present: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pragma_table_info(?) WHERE name = ?)")
                .bind(table)
                .bind(column)
                .fetch_one(pool)
                .await?;
        if !present {
            sqlx::query(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition}"
            ))
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Orders definitions so every parent comes before its children, so writing them in order
/// never trips a `parent_id` foreign key. Rows in a parent cycle go last.
fn parents_first<T>(rows: Vec<T>, id_and_parent: impl Fn(&T) -> (&str, Option<&str>)) -> Vec<T> {
    let parents: HashMap<&str, Option<&str>> = rows.iter().map(&id_and_parent).collect();
    let depth = |row: &T| {
        let mut parent = id_and_parent(row).1;
        let mut depth = 0;
        while let Some(p) = parent.filter(|_| depth <= rows.len()) {
            depth += 1;
            parent = parents.get(p).copied().flatten();
        }
        depth
    };
    let mut keyed: Vec<(usize, usize)> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| (depth(r), i))
        .collect();
    keyed.sort();
    let mut slots: Vec<Option<T>> = rows.into_iter().map(Some).collect();
    keyed
        .into_iter()
        .filter_map(|(_, i)| slots[i].take())
        .collect()
}

/// Connect to a per-library SQLite database with NAS-safe settings.
/// WAL mode is intentionally avoided: on SMB/NFS shares the -shm shared-memory
/// locking that WAL requires is unreliable and can silently corrupt the database
/// when multiple machines write concurrently. DELETE (rollback) journal is the
/// SQLite-recommended safe choice for any network filesystem.
pub async fn connect_library_sqlite(db_path: &Path) -> Result<Pool<Sqlite>> {
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Delete) // safe on SMB/NFS; WAL is not
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(30)); // 30s for slow NAS writes

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect_with(options)
        .await?;

    // In-memory temp store and 64MB cache in RAM for limited NAS read/write
    sqlx::query(
        r#"
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -64000;
        PRAGMA foreign_keys = OFF;
        "#,
    )
    .execute(&pool)
    .await?;

    create_library_schema(&pool).await?;

    // Backward compatibility: ensure dir_mtimes table exists
    let _ = sqlx::query(
        "CREATE TABLE IF NOT EXISTS dir_mtimes (dir_path TEXT PRIMARY KEY, mtime INTEGER NOT NULL)",
    )
    .execute(&pool)
    .await;

    // Backward compatibility migration for subcollections
    let _ = sqlx::query(
        "ALTER TABLE collections ADD COLUMN parent_id TEXT REFERENCES collections(id) ON DELETE CASCADE"
    )
    .execute(&pool)
    .await;

    // Backward compatibility migration for is_favorite
    let _ = sqlx::query("ALTER TABLE assets ADD COLUMN is_favorite INTEGER NOT NULL DEFAULT 0")
        .execute(&pool)
        .await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_assets_is_favorite ON assets(is_favorite)")
        .execute(&pool)
        .await;
    add_tag_description_column(&pool)
        .await
        .context("Could not add the tag description column")?;
    add_structure_columns(&pool)
        .await
        .context("Could not add the support role and group columns")?;
    add_preview_columns(&pool)
        .await
        .context("Could not add the preview version columns")?;
    add_pack_override_columns(&pool)
        .await
        .context("Could not add the pack origin columns")?;
    // Search still works on the old index, so a failed conversion only waits for the next open.
    if let Err(e) = index_file_paths(&pool, false).await {
        tracing::warn!("Could not index folder names for search in {db_path:?}: {e}");
    }

    // Purge any existing Revit backup files from database
    if let Ok(assets) = sqlx::query_as::<_, (String, String)>(
        "SELECT id, file_name FROM assets WHERE extension IN ('rfa', 'rvt')",
    )
    .fetch_all(&pool)
    .await
    {
        let mut purged = false;
        for (id, name) in assets {
            if crate::watcher::is_revit_backup(&name) {
                let _ = sqlx::query("DELETE FROM assets WHERE id = ?")
                    .bind(&id)
                    .execute(&pool)
                    .await;
                purged = true;
            }
        }
        if purged {
            let _ = sqlx::query("UPDATE libraries SET asset_count = (SELECT COUNT(*) FROM assets)")
                .execute(&pool)
                .await;
        }
    }

    Ok(pool)
}

/// Opens the DB of the library rooted at `root` (`connect_library_sqlite`) and, before the
/// library is registered, takes its `index_lock` (`Database::index_lock`), which waits for a
/// build of this app, then its advisory lock. Holding both, it runs swap recovery and the
/// structure pass, so a library has its folder tree from its first open even when the index run
/// that follows fails, and no pass runs between a pack's swap and its commit. While another
/// holder has the advisory lock it runs neither and leaves the pass pending: that holder's
/// release, or the next index run or open, runs it.
pub async fn open_library_db(
    index_lock: &tokio::sync::Mutex<()>,
    root: &Path,
) -> Result<Pool<Sqlite>> {
    let db_path = root.join(".opendam").join("library.sqlite");
    let pool = connect_library_sqlite(&db_path).await?;
    let index_guard = index_lock.lock().await;
    let Some(advisory) = crate::watcher::IndexLock::try_acquire(root).await else {
        tracing::info!(
            "Another holder has the advisory lock of {root:?}: its structure pass runs later"
        );
        crate::structure::mark_pending(&pool).await;
        return Ok(pool);
    };
    crate::import::recover_swaps(
        &pool,
        root,
        crate::import::Recovery::Open,
        &index_guard,
        &advisory,
    )
    .await;
    if let Err(e) = crate::structure::rebuild(&pool, root).await {
        tracing::warn!("Could not derive the folder tree of {db_path:?}: {e}");
    }
    advisory.release(&pool, root).await;
    Ok(pool)
}

/// Tables only the master DB has: the registered libraries, non-secret settings as
/// key/JSON-value rows (API keys live in the OS keyring, never here), and auto-tagging runs.
async fn create_master_tables(master_pool: &Pool<Sqlite>) -> sqlx::Result<()> {
    sqlx::query(crate::autotag::store::SCHEMA)
        .execute(master_pool)
        .await?;
    add_run_columns(master_pool).await?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS libraries (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            root_paths  TEXT NOT NULL,
            created_at  INTEGER NOT NULL,
            asset_count INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )
    .execute(master_pool)
    .await
    .map(|_| ())
}

impl Database {
    pub async fn new(master_db_url: &str) -> Result<Self> {
        let master_path = Path::new(master_db_url);
        let master_options = SqliteConnectOptions::new()
            .filename(master_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Delete) // consistent with library DBs
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(std::time::Duration::from_secs(30));

        let master_pool = SqlitePoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect_with(master_options)
            .await?;

        create_master_tables(&master_pool).await?;

        // Also ensure library schema tables exist on master_pool as fallback
        let _ = create_library_schema(&master_pool).await;

        let _ = sqlx::query("ALTER TABLE assets ADD COLUMN is_favorite INTEGER NOT NULL DEFAULT 0")
            .execute(&master_pool)
            .await;
        let _ =
            sqlx::query("CREATE INDEX IF NOT EXISTS idx_assets_is_favorite ON assets(is_favorite)")
                .execute(&master_pool)
                .await;
        add_tag_description_column(&master_pool)
            .await
            .context("Could not add the tag description column to the master DB")?;

        let db = Self {
            master_pool,
            libraries: Default::default(),
            index_locks: Default::default(),
            catalog_lock: Default::default(),
            late_passes: tokio::sync::broadcast::channel(16).0,
            follow_ups: Default::default(),
        };

        // Load all registered libraries and open their per-library SQLite pools
        if let Ok(rows) = sqlx::query_as::<_, (String, String, String)>(
            "SELECT id, name, root_paths FROM libraries ORDER BY id",
        )
        .fetch_all(&db.master_pool)
        .await
        {
            for (id, name, root_paths_json) in rows {
                let paths: Vec<String> = serde_json::from_str(&root_paths_json).unwrap_or_default();
                if let Some(first_path) = paths.first() {
                    let root_path = PathBuf::from(first_path);
                    if root_path.exists() {
                        let opendam_dir = root_path.join(".opendam");
                        let thumbs_dir = opendam_dir.join("thumbs");
                        let _ = tokio::fs::create_dir_all(&thumbs_dir).await;

                        match open_library_db(&db.index_lock(&id), &root_path).await {
                            Ok(lib_pool) => {
                                db.libraries.write().await.insert(
                                    id.clone(),
                                    LibraryHandle {
                                        id,
                                        name,
                                        root_path,
                                        pool: lib_pool,
                                    },
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to open library DB at {:?}: {}",
                                    opendam_dir.join("library.sqlite"),
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }

        db.sync_open_libraries().await;

        Ok(db)
    }

    /// Runs when libraries open. Master first gains the definitions it lacks from every open
    /// library's mirror, in library-id order so the winner between two mirrors doesn't depend
    /// on HashMap order (insert-only, so a rename made while a library was offline is kept).
    /// Only then is every mirror overwritten from master and cleared of memberships for assets
    /// its library doesn't hold, so no mirror misses a definition a later library brought.
    pub async fn sync_open_libraries(&self) {
        let libs = self.get_all_libraries().await;
        for lib in &libs {
            self.add_definitions_master_lacks(&lib.pool).await;
        }
        for lib in &libs {
            if let Err(e) = self.converge_mirror(&lib.pool).await {
                tracing::error!("Failed to update library {}'s mirror: {}", lib.id, e);
            }
        }
    }

    async fn add_definitions_master_lacks(&self, lib_pool: &Pool<Sqlite>) {
        if let Ok(tags) =
            sqlx::query_as::<_, TagRow>("SELECT id, name, color, parent_id, description FROM tags")
                .fetch_all(lib_pool)
                .await
        {
            for (id, name, color, parent_id, description) in tags {
                let _ = sqlx::query(
                    "INSERT OR IGNORE INTO tags (id, name, color, parent_id, description)
                     VALUES (?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(&name)
                .bind(&color)
                .bind(&parent_id)
                .bind(&description)
                .execute(&self.master_pool)
                .await;
            }
        }

        if let Ok(cols) = sqlx::query_as::<_, CollectionRow>(
            "SELECT id, name, description, parent_id, is_smart, filter_spec, created_at FROM collections"
        ).fetch_all(lib_pool).await {
            for (id, name, desc, parent_id, is_smart, filter_spec, created_at) in cols {
                let _ = sqlx::query("INSERT OR IGNORE INTO collections (id, name, description, parent_id, is_smart, filter_spec, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
                    .bind(&id).bind(&name).bind(&desc).bind(&parent_id).bind(is_smart).bind(&filter_spec).bind(created_at)
                    .execute(&self.master_pool).await;
            }
        }
    }

    async fn converge_mirror(&self, lib_pool: &Pool<Sqlite>) -> sqlx::Result<()> {
        let tags =
            sqlx::query_as::<_, TagRow>("SELECT id, name, color, parent_id, description FROM tags")
                .fetch_all(&self.master_pool)
                .await?;
        let cols = sqlx::query_as::<_, CollectionRow>(
            "SELECT id, name, description, parent_id, is_smart, filter_spec, created_at FROM collections",
        )
        .fetch_all(&self.master_pool)
        .await?;

        let mut tx = lib_pool.begin().await?;
        for tag in parents_first(tags, |t| (&t.0, t.3.as_deref())) {
            // `tags.name` is unique: if master's name is another mirror tag's, leave both.
            if let Err(e) = upsert_tag(&mut *tx, &tag).await {
                tracing::warn!("Left mirror tag {} ({:?}) as it was: {}", tag.0, tag.1, e);
            }
        }
        for col in parents_first(cols, |c| (&c.0, c.3.as_deref())) {
            if let Err(e) = upsert_collection(&mut *tx, &col).await {
                tracing::warn!("Left mirror collection {} as it was: {}", col.0, e);
            }
        }
        sqlx::query("DELETE FROM asset_tags WHERE asset_id NOT IN (SELECT id FROM assets)")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM collection_assets WHERE asset_id NOT IN (SELECT id FROM assets)")
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }

    pub async fn register_library(
        &self,
        id: &str,
        name: &str,
        root_path: &Path,
    ) -> Result<LibraryHandle> {
        let opendam_dir = root_path.join(".opendam");
        let thumbs_dir = opendam_dir.join("thumbs");
        tokio::fs::create_dir_all(&thumbs_dir).await?;

        let lib_pool = open_library_db(&self.index_lock(id), root_path).await?;

        let handle = LibraryHandle {
            id: id.to_string(),
            name: name.to_string(),
            root_path: root_path.to_path_buf(),
            pool: lib_pool,
        };

        self.libraries
            .write()
            .await
            .insert(id.to_string(), handle.clone());
        // The new library may bring definitions master lacked; the other open mirrors get
        // them too, so tagging an asset elsewhere never names a tag its mirror doesn't have.
        self.sync_open_libraries().await;

        Ok(handle)
    }

    pub async fn unregister_library(&self, id: &str) -> Result<()> {
        let mut libs = self.libraries.write().await;
        libs.remove(id);
        Ok(())
    }

    /// Returns library `id` only if it is open, never another one: libraries whose root is
    /// missing are not opened, and handing their callers another library's pool lets a
    /// re-index delete that library's rows.
    pub async fn get_library_handle(&self, id: &str) -> Option<LibraryHandle> {
        self.libraries.read().await.get(id).cloned()
    }

    /// The lock that serializes library `id`'s index runs and structure passes within this app:
    /// an index run holds it from its walk to its structure pass, so a command's pass waits for
    /// the run and never writes over the run's result with one derived from older rows.
    pub fn index_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.index_locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.entry(id.to_string()).or_default().clone()
    }

    /// Every open library, in library-id order.
    pub async fn get_all_libraries(&self) -> Vec<LibraryHandle> {
        let mut libs: Vec<LibraryHandle> = self.libraries.read().await.values().cloned().collect();
        libs.sort_by(|a, b| a.id.cmp(&b.id));
        libs
    }

    /// Tag `id` and its ancestors from the master catalog, root first and the tag itself last,
    /// so writing them in order never trips `parent_id`. Empty when master lacks the tag.
    pub async fn tag_with_ancestors(&self, id: &str) -> sqlx::Result<Vec<TagRow>> {
        sqlx::query_as(
            "WITH RECURSIVE chain(id, name, color, parent_id, description, depth) AS (
                 SELECT id, name, color, parent_id, description, 0 FROM tags WHERE id = ?
                 UNION ALL
                 SELECT t.id, t.name, t.color, t.parent_id, t.description, chain.depth + 1
                 FROM tags t JOIN chain ON t.id = chain.parent_id
                 WHERE chain.depth < 64
             )
             SELECT id, name, color, parent_id, description FROM chain ORDER BY depth DESC",
        )
        .bind(id)
        .fetch_all(&self.master_pool)
        .await
    }

    /// Collection `id` and its ancestors from the master catalog, root first, like
    /// `tag_with_ancestors`. Empty when master lacks the collection.
    pub async fn collection_with_ancestors(&self, id: &str) -> sqlx::Result<Vec<CollectionRow>> {
        sqlx::query_as(
            "WITH RECURSIVE chain(id, name, description, parent_id, is_smart, filter_spec, created_at, depth) AS (
                 SELECT id, name, description, parent_id, is_smart, filter_spec, created_at, 0
                 FROM collections WHERE id = ?
                 UNION ALL
                 SELECT c.id, c.name, c.description, c.parent_id, c.is_smart, c.filter_spec,
                        c.created_at, chain.depth + 1
                 FROM collections c JOIN chain ON c.id = chain.parent_id
                 WHERE chain.depth < 64
             )
             SELECT id, name, description, parent_id, is_smart, filter_spec, created_at
             FROM chain ORDER BY depth DESC",
        )
        .bind(id)
        .fetch_all(&self.master_pool)
        .await
    }

    /// The open library that holds asset `asset_id`: the only library its tag and collection
    /// memberships are written to. A library that can't be read is an error rather than a
    /// "not here", so a busy NAS never sends a write to another library or drops it.
    pub async fn library_of_asset(&self, asset_id: &str) -> sqlx::Result<Option<LibraryHandle>> {
        for lib in self.get_all_libraries().await {
            let found: Option<String> = sqlx::query_scalar("SELECT id FROM assets WHERE id = ?")
                .bind(asset_id)
                .fetch_optional(&lib.pool)
                .await?;
            if found.is_some() {
                return Ok(Some(lib));
            }
        }
        Ok(None)
    }
}

/// Helper to resolve a stored relative path to an absolute path for the frontend
pub fn resolve_path(root: &Path, rel_or_abs: &str) -> String {
    let p = Path::new(rel_or_abs);
    if p.is_absolute() {
        rel_or_abs.to_string()
    } else {
        root.join(p).to_string_lossy().to_string()
    }
}

/// Helper to convert an absolute path to a relative path for storage
pub fn make_relative_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

/// An in-memory database with the library schema, for tests, with `foreign_keys` pinned off.
/// One connection, since every connection to `sqlite::memory:` opens its own empty database.
#[cfg(test)]
pub(crate) async fn memory_library_pool() -> Pool<Sqlite> {
    memory_pool(false).await
}

/// Like `memory_library_pool`, but enforcing foreign keys as most production connections do.
#[cfg(test)]
pub(crate) async fn memory_library_pool_enforcing_foreign_keys() -> Pool<Sqlite> {
    memory_pool(true).await
}

#[cfg(test)]
async fn memory_pool(foreign_keys: bool) -> Pool<Sqlite> {
    use std::str::FromStr;
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(foreign_keys);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    create_library_schema(&pool).await.unwrap();
    add_structure_columns(&pool).await.unwrap();
    add_preview_columns(&pool).await.unwrap();
    add_pack_override_columns(&pool).await.unwrap();
    // The schema script sets the pragma itself, so pin it again afterwards.
    sqlx::query(&format!(
        "PRAGMA foreign_keys = {}",
        if foreign_keys { "ON" } else { "OFF" }
    ))
    .execute(&pool)
    .await
    .unwrap();
    pool
}

/// The full-text table and triggers of libraries made before `file_path` was indexed.
#[cfg(test)]
const OLD_ASSETS_FTS_SCHEMA: &str = "
    DROP TRIGGER assets_ai;
    DROP TRIGGER assets_ad;
    DROP TRIGGER assets_au;
    DROP TABLE assets_fts;
    CREATE VIRTUAL TABLE assets_fts USING fts5(
        id UNINDEXED, file_name, description, metadata, content='assets', content_rowid='rowid'
    );
    CREATE TRIGGER assets_ai AFTER INSERT ON assets BEGIN
        INSERT INTO assets_fts(rowid, id, file_name, description, metadata)
        VALUES (new.rowid, new.id, new.file_name, new.description, new.metadata);
    END;
    CREATE TRIGGER assets_ad AFTER DELETE ON assets BEGIN
        INSERT INTO assets_fts(assets_fts, rowid, id, file_name, description, metadata)
        VALUES ('delete', old.rowid, old.id, old.file_name, old.description, old.metadata);
    END;
    CREATE TRIGGER assets_au AFTER UPDATE ON assets BEGIN
        INSERT INTO assets_fts(assets_fts, rowid, id, file_name, description, metadata)
        VALUES ('delete', old.rowid, old.id, old.file_name, old.description, old.metadata);
        INSERT INTO assets_fts(rowid, id, file_name, description, metadata)
        VALUES (new.rowid, new.id, new.file_name, new.description, new.metadata);
    END;";

/// Like `memory_library_pool`, but with the full-text table and triggers libraries had before
/// `file_path` was indexed, so `index_file_paths` has a library to convert.
#[cfg(test)]
pub(crate) async fn memory_library_pool_with_old_fts() -> Pool<Sqlite> {
    let pool = memory_library_pool().await;
    sqlx::query(OLD_ASSETS_FTS_SCHEMA)
        .execute(&pool)
        .await
        .unwrap();
    pool
}

/// The rowids the full-text index itself matches for the FTS5 query `fts`, read without
/// `assets`, so a row whose index entries outlived it still shows. Fails the test unless the
/// index agrees with `assets`, which a trigger's `'delete'` with values that were never indexed
/// breaks.
#[cfg(test)]
pub(crate) async fn indexed_rowids(pool: &Pool<Sqlite>, fts: &str) -> Vec<i64> {
    sqlx::query("INSERT INTO assets_fts(assets_fts, rank) VALUES ('integrity-check', 1)")
        .execute(pool)
        .await
        .expect("the full-text index disagrees with assets");
    sqlx::query_scalar("SELECT rowid FROM assets_fts WHERE assets_fts MATCH ? ORDER BY rowid")
        .bind(fts)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// A database over an in-memory master and the given open libraries, for tests.
#[cfg(test)]
pub(crate) async fn memory_database(libraries: Vec<LibraryHandle>) -> Database {
    let master_pool = memory_library_pool().await;
    create_master_tables(&master_pool).await.unwrap();
    Database {
        master_pool,
        libraries: Arc::new(RwLock::new(
            libraries.into_iter().map(|h| (h.id.clone(), h)).collect(),
        )),
        index_locks: Default::default(),
        catalog_lock: Default::default(),
        late_passes: tokio::sync::broadcast::channel(16).0,
        follow_ups: Default::default(),
    }
}

/// An open library over `pool`, for tests.
#[cfg(test)]
pub(crate) fn memory_library(id: &str, pool: Pool<Sqlite>) -> LibraryHandle {
    LibraryHandle {
        id: id.to_string(),
        name: id.to_string(),
        root_path: PathBuf::from(format!("/mnt/{id}")),
        pool,
    }
}

/// Inserts an asset row with `id`, for tests.
#[cfg(test)]
pub(crate) async fn insert_asset(pool: &Pool<Sqlite>, id: &str) {
    sqlx::query(
        "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
         VALUES (?, ?, ?, 'glb', 'model', 0, 0, 0)",
    )
    .bind(id)
    .bind(format!("models/{id}.glb"))
    .bind(format!("{id}.glb"))
    .execute(pool)
    .await
    .unwrap();
}

/// Inserts a tag definition, for tests.
#[cfg(test)]
pub(crate) async fn insert_tag(pool: &Pool<Sqlite>, id: &str, name: &str) {
    sqlx::query("INSERT INTO tags (id, name) VALUES (?, ?)")
        .bind(id)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
}

/// Inserts a collection definition, for tests.
#[cfg(test)]
pub(crate) async fn insert_collection(
    pool: &Pool<Sqlite>,
    id: &str,
    name: &str,
    parent_id: Option<&str>,
) {
    sqlx::query("INSERT INTO collections (id, name, parent_id, created_at) VALUES (?, ?, ?, 0)")
        .bind(id)
        .bind(name)
        .bind(parent_id)
        .execute(pool)
        .await
        .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_joins_relative_paths_to_the_root() {
        let root = Path::new("/mnt/assets");
        assert_eq!(
            resolve_path(root, "textures/rock.png"),
            "/mnt/assets/textures/rock.png"
        );
    }

    #[test]
    fn resolve_path_keeps_absolute_paths() {
        let root = Path::new("/mnt/assets");
        assert_eq!(resolve_path(root, "/tmp/rock.png"), "/tmp/rock.png");
    }

    #[test]
    fn make_relative_path_strips_the_root() {
        let root = Path::new("/mnt/assets");
        assert_eq!(
            make_relative_path(root, Path::new("/mnt/assets/textures/rock.png")),
            "textures/rock.png"
        );
    }

    #[test]
    fn make_relative_path_keeps_paths_outside_the_root() {
        let root = Path::new("/mnt/assets");
        assert_eq!(
            make_relative_path(root, Path::new("/tmp/rock.png")),
            "/tmp/rock.png"
        );
    }

    #[test]
    fn relative_paths_round_trip() {
        let root = Path::new("/mnt/assets");
        let rel = make_relative_path(root, Path::new("/mnt/assets/models/tree.glb"));
        assert_eq!(resolve_path(root, &rel), "/mnt/assets/models/tree.glb");
    }

    #[tokio::test]
    async fn library_schema_indexes_assets_for_full_text_search() {
        let pool = memory_library_pool().await;
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
             VALUES ('a1', 'models/tree.glb', 'tree.glb', 'glb', 'model', 1024, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM assets_fts WHERE assets_fts MATCH 'tree*'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(ids, ["a1"]);
    }

    async fn has_file_path_column(pool: &Pool<Sqlite>) -> bool {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('assets_fts') WHERE name = 'file_path')",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn fts_conversion_record(pool: &Pool<Sqlite>) -> Option<String> {
        sqlx::query_scalar("SELECT value FROM library_meta WHERE key = ?")
            .bind(FTS_FILE_PATH_KEY)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_new_library_indexes_file_paths_with_no_conversion() {
        let pool = memory_library_pool().await;
        insert_asset(&pool, "crate").await;

        assert!(!index_file_paths(&pool, false).await.unwrap());
        assert_eq!(fts_conversion_record(&pool).await, None);
        assert_eq!(indexed_rowids(&pool, "models").await, [1]);
    }

    #[tokio::test]
    async fn every_trigger_writes_the_file_path() {
        let pool = memory_library_pool().await;
        let triggers: Vec<String> = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'assets'
             ORDER BY name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(triggers.len(), 3);
        for sql in triggers {
            assert!(sql.contains(".file_path"), "{sql}");
        }
    }

    #[tokio::test]
    async fn only_writes_to_indexed_columns_touch_the_full_text_index() {
        let pool = memory_library_pool().await;
        insert_asset(&pool, "crate").await;
        let index = || async {
            sqlx::query_as::<_, (i64, Vec<u8>)>("SELECT id, block FROM assets_fts_data ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap()
        };
        let before = index().await;

        sqlx::query(
            "UPDATE assets SET rating = 5, color_label = 'red', is_favorite = 1, size_bytes = 9,
                 modified_at = 1, indexed_at = 1, thumbnail_path = 'x', preview_status = 'done'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(index().await, before);

        // Each indexed column alone reindexes, and the trigger's 'delete' removes its old value.
        for column in ["file_name", "description", "metadata", "file_path"] {
            let word = column.replace('_', "");
            for value in ["first", "second"] {
                sqlx::query(&format!("UPDATE assets SET {column} = '{value}{word}'"))
                    .execute(&pool)
                    .await
                    .unwrap();
                assert_eq!(
                    indexed_rowids(&pool, &format!("{value}{word}")).await,
                    [1],
                    "{column}"
                );
            }
            assert_eq!(
                indexed_rowids(&pool, &format!("first{word}")).await,
                Vec::<i64>::new(),
                "{column}"
            );
        }
    }

    #[tokio::test]
    async fn an_interrupted_conversion_runs_again_on_the_next_open_and_never_twice() {
        let dir = std::env::temp_dir().join(format!("opendam-fts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("library.sqlite");
        let old = connect_library_sqlite(&file).await.unwrap();
        sqlx::query(OLD_ASSETS_FTS_SCHEMA)
            .execute(&old)
            .await
            .unwrap();
        insert_asset(&old, "crate").await;

        assert!(!index_file_paths(&old, true).await.unwrap());
        assert!(!has_file_path_column(&old).await);
        assert_eq!(fts_conversion_record(&old).await, None);
        assert_eq!(indexed_rowids(&old, "crate").await, [1]);
        assert_eq!(indexed_rowids(&old, "models").await, Vec::<i64>::new());
        old.close().await;

        let reopened = connect_library_sqlite(&file).await.unwrap();
        let converted = (
            has_file_path_column(&reopened).await,
            fts_conversion_record(&reopened).await,
            indexed_rowids(&reopened, "models").await,
            index_file_paths(&reopened, false).await.unwrap(),
        );
        reopened.close().await;
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(converted, (true, Some("done".to_string()), vec![1], false));
    }

    #[tokio::test]
    async fn opening_a_library_derives_its_folder_tree() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".opendam/library.sqlite");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let pool = connect_library_sqlite(&file).await.unwrap();
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
             VALUES ('a', 'kits/rocks/rock.glb', 'rock.glb', 'glb', '3d_model', 1, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let reopened = open_library_db(&Default::default(), dir.path())
            .await
            .unwrap();

        assert_eq!(
            crate::structure::tests::derived_rows(&reopened).await,
            crate::structure::tests::fresh_pass(&["kits/rocks/rock.glb"]).await
        );
    }

    /// A library folder whose DB holds a row for `kits/rocks/rock.glb`, on disk too, and no
    /// structure yet.
    async fn library_without_structure() -> (tempfile::TempDir, PathBuf) {
        // Not `tempfile::tempdir()`: the walker skips hidden dirs, and that names it `.tmpXXXX`.
        let dir = tempfile::Builder::new()
            .prefix("opendam-lib-")
            .tempdir()
            .unwrap();
        let root = dir.path().join("lib");
        std::fs::create_dir_all(root.join("kits/rocks")).unwrap();
        std::fs::create_dir_all(root.join(".opendam/thumbs")).unwrap();
        std::fs::write(root.join("kits/rocks/rock.glb"), "glTF").unwrap();
        let pool = connect_library_sqlite(&root.join(".opendam/library.sqlite"))
            .await
            .unwrap();
        crate::structure::tests::insert_path(&pool, "kits/rocks/rock.glb").await;
        pool.close().await;
        (dir, root)
    }

    #[tokio::test]
    async fn an_open_while_another_machine_holds_the_lock_leaves_its_pass_to_the_next_open() {
        use crate::structure::tests::{derived_rows, fresh_pass};
        let (_dir, root) = library_without_structure().await;
        let other_machine = crate::watcher::IndexLock::try_acquire(&root).await.unwrap();

        let opened = open_library_db(&Default::default(), &root).await.unwrap();
        assert!(
            derived_rows(&opened).await.folders.is_empty(),
            "the pass ran while another holder had the lock"
        );
        assert!(crate::structure::is_pending(&opened).await);

        // It goes without a release, as if its process died.
        drop(other_machine);
        let reopened = open_library_db(&Default::default(), &root).await.unwrap();
        assert_eq!(
            derived_rows(&reopened).await,
            fresh_pass(&["kits/rocks/rock.glb"]).await
        );
        assert!(!crate::structure::is_pending(&reopened).await);
    }

    #[tokio::test]
    async fn an_open_while_another_machine_holds_the_lock_leaves_its_pass_to_the_next_index_run() {
        use crate::structure::tests::{derived_rows, fresh_pass};
        let (dir, root) = library_without_structure().await;
        let other_machine = crate::watcher::IndexLock::try_acquire(&root).await.unwrap();
        let db = Database::new(dir.path().join("master.sqlite").to_str().unwrap())
            .await
            .unwrap();
        let pool = db.register_library("lib", "lib", &root).await.unwrap().pool;
        let paths = [root.display().to_string()];
        let index = || crate::watcher::index_paths(&db, "lib", &paths);

        index().await.unwrap();
        assert!(
            derived_rows(&pool).await.folders.is_empty(),
            "the open or the index run ran a pass while another holder had the lock"
        );

        drop(other_machine);
        index().await.unwrap();
        assert_eq!(
            derived_rows(&pool).await,
            fresh_pass(&["kits/rocks/rock.glb"]).await
        );
        assert!(!crate::structure::is_pending(&pool).await);
    }

    #[tokio::test]
    async fn opening_a_library_made_before_support_roles_and_groups_adds_them_and_derives_them() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".opendam/library.sqlite");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let pool = connect_library_sqlite(&file).await.unwrap();
        let paths = [
            "kits/rocks/License.txt",
            "kits/rocks/GLB format/rock.glb",
            "kits/rocks/OBJ format/rock.obj",
            "kits/rocks/Previews/rock.png",
        ];
        for path in paths {
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
                 VALUES (?, ?, 'f', 'glb', '3d_model', 1, 0, 0)",
            )
            .bind(path)
            .bind(path)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "DROP INDEX idx_assets_items; DROP INDEX idx_assets_group_id;
             ALTER TABLE assets DROP COLUMN bundled_preview; ALTER TABLE assets DROP COLUMN group_id;
             ALTER TABLE assets DROP COLUMN support_role; DELETE FROM folders;
             ALTER TABLE folders DROP COLUMN split_groups;",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let reopened = open_library_db(&Default::default(), dir.path())
            .await
            .unwrap();

        let derived = crate::structure::tests::derived_rows(&reopened).await;
        assert_eq!(derived, crate::structure::tests::fresh_pass(&paths).await);
        assert!(derived
            .roles
            .contains(&(paths[0].to_string(), Some("doc".to_string()))));
        let glb = Some(paths[1].to_string());
        assert_eq!(
            derived.groups,
            [
                (paths[1].to_string(), glb.clone()),
                (paths[2].to_string(), glb)
            ]
        );
        assert_eq!(
            derived.bundled_previews,
            [(paths[1].to_string(), paths[3].to_string())]
        );
    }

    #[tokio::test]
    async fn opening_a_library_made_before_render_sizes_reads_them_and_rerenders_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".opendam/library.sqlite");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let pool = connect_library_sqlite(&file).await.unwrap();
        // As plan C12 left a Kenney kit: each model records its 64 px render and is off the
        // queue. The sedan was rendered before C12, the van never was.
        let (sedan, van) = ("kit/Models/sedan.glb", "kit/Models/van.glb");
        for (path, status, thumbnail, role, render) in [
            (
                sedan,
                "done",
                Some(".opendam/thumbs/sedan_thumb.png"),
                None,
                Some("kit/Previews/sedan.png"),
            ),
            (van, "none", None, None, Some("kit/Previews/van.png")),
            (
                "kit/Previews/sedan.png",
                "done",
                None,
                Some("model_preview"),
                None,
            ),
            (
                "kit/Previews/van.png",
                "done",
                None,
                Some("model_preview"),
                None,
            ),
        ] {
            if role.is_some() {
                crate::structure::tests::write_png(&dir.path().join(path), 64, 64);
            }
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, preview_status, thumbnail_path, support_role, bundled_preview)
                 VALUES (?, ?, 'f', ?, ?, 1, 0, 0, ?, ?, ?, ?)",
            )
            .bind(path)
            .bind(path)
            .bind(path.rsplit('.').next().unwrap())
            .bind(if path.ends_with(".glb") { "3d_model" } else { "texture" })
            .bind(status)
            .bind(thumbnail)
            .bind(role)
            .bind(render)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "ALTER TABLE assets DROP COLUMN render_width; ALTER TABLE assets DROP COLUMN render_height;
             ALTER TABLE assets DROP COLUMN render_read_bytes;
             ALTER TABLE assets DROP COLUMN render_read_mtime;",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let reopened = open_library_db(&Default::default(), dir.path())
            .await
            .unwrap();

        // The pass at open reads both renders: too small, so the van is queued, and the sedan
        // keeps the thumbnail it has. Nothing else changes, and a rerun reads nothing.
        use crate::structure::tests::render_read;
        for render in ["kit/Previews/sedan.png", "kit/Previews/van.png"] {
            assert_eq!(
                render_read(&reopened, render).await,
                [Some(64), Some(64), Some(1), Some(0)]
            );
        }
        let models: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT file_path, preview_status, thumbnail_path FROM assets
             WHERE extension = 'glb' ORDER BY file_path",
        )
        .fetch_all(&reopened)
        .await
        .unwrap();
        assert_eq!(
            models,
            [
                (
                    sedan.to_string(),
                    "done".to_string(),
                    Some(".opendam/thumbs/sedan_thumb.png".to_string())
                ),
                (van.to_string(), "pending".to_string(), None),
            ]
        );
        let jobs: Vec<String> = sqlx::query_scalar("SELECT asset_id FROM preview_jobs")
            .fetch_all(&reopened)
            .await
            .unwrap();
        assert_eq!(jobs, [van]);
        assert_eq!(
            crate::structure::pass(&reopened, dir.path()).await.unwrap(),
            crate::structure::PassRun {
                written: 0,
                render_reads: 0
            }
        );
        assert_eq!(
            crate::structure::tests::derived_rows(&reopened)
                .await
                .bundled_previews,
            [
                (sedan.to_string(), "kit/Previews/sedan.png".to_string()),
                (van.to_string(), "kit/Previews/van.png".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn get_library_handle_does_not_fall_back_to_another_library() {
        let db = memory_database(vec![memory_library("open", memory_library_pool().await)]).await;

        assert!(db.get_library_handle("offline").await.is_none());
        assert_eq!(db.get_library_handle("open").await.unwrap().id, "open");
    }

    async fn tag_name(pool: &Pool<Sqlite>, id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT name FROM tags WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    async fn collection_name(pool: &Pool<Sqlite>, id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT name FROM collections WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    async fn count(pool: &Pool<Sqlite>, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    #[tokio::test]
    async fn opening_a_library_adds_what_master_lacks_then_overwrites_its_mirror_from_master() {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        let db =
            memory_database(vec![memory_library("a", a.clone()), memory_library("b", b)]).await;
        insert_tag(&db.master_pool, "t", "new").await;
        insert_tag(&a, "t", "old").await;
        insert_tag(&a, "u", "only-in-a").await;

        db.sync_open_libraries().await;

        assert_eq!(tag_name(&db.master_pool, "t").await.as_deref(), Some("new"));
        assert_eq!(
            tag_name(&db.master_pool, "u").await.as_deref(),
            Some("only-in-a")
        );
        assert_eq!(tag_name(&a, "t").await.as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn opening_a_library_keeps_its_memberships_and_child_collections() {
        let a = memory_library_pool_enforcing_foreign_keys().await;
        let db = memory_database(vec![memory_library("a", a.clone())]).await;
        insert_asset(&a, "asset").await;
        insert_tag(&a, "t", "old tag").await;
        insert_collection(&a, "c", "old collection", None).await;
        insert_collection(&a, "c2", "child", Some("c")).await;
        sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES ('asset', 't')")
            .execute(&a)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO collection_assets (collection_id, asset_id) VALUES ('c', 'asset')",
        )
        .execute(&a)
        .await
        .unwrap();
        insert_tag(&db.master_pool, "t", "new tag").await;
        insert_collection(&db.master_pool, "c", "new collection", None).await;

        db.sync_open_libraries().await;

        assert_eq!(tag_name(&a, "t").await.as_deref(), Some("new tag"));
        assert_eq!(
            collection_name(&a, "c").await.as_deref(),
            Some("new collection")
        );
        assert_eq!(count(&a, "SELECT COUNT(*) FROM asset_tags").await, 1);
        assert_eq!(count(&a, "SELECT COUNT(*) FROM collection_assets").await, 1);
        assert_eq!(collection_name(&a, "c2").await.as_deref(), Some("child"));
    }

    #[tokio::test]
    async fn opening_a_library_mirrors_a_child_collection_master_stores_before_its_parent() {
        let a = memory_library_pool_enforcing_foreign_keys().await;
        let db = memory_database(vec![memory_library("a", a.clone())]).await;
        insert_collection(&db.master_pool, "child", "child", Some("parent")).await;
        insert_collection(&db.master_pool, "parent", "parent", None).await;

        db.sync_open_libraries().await;

        assert_eq!(
            collection_name(&a, "parent").await.as_deref(),
            Some("parent")
        );
        assert_eq!(collection_name(&a, "child").await.as_deref(), Some("child"));
    }

    #[tokio::test]
    async fn opening_a_library_leaves_a_tag_whose_master_name_collides_in_its_mirror() {
        let a = memory_library_pool().await;
        let db = memory_database(vec![memory_library("a", a.clone())]).await;
        insert_tag(&a, "t", "rock").await;
        insert_tag(&a, "u", "stone").await;
        insert_tag(&db.master_pool, "t", "stone").await;

        db.sync_open_libraries().await;

        assert_eq!(tag_name(&a, "t").await.as_deref(), Some("rock"));
        assert_eq!(tag_name(&a, "u").await.as_deref(), Some("stone"));
    }

    #[tokio::test]
    async fn opening_a_library_removes_memberships_whose_asset_it_does_not_hold() {
        let a = memory_library_pool().await;
        let db = memory_database(vec![memory_library("a", a.clone())]).await;
        insert_asset(&a, "here").await;
        insert_tag(&a, "t", "rock").await;
        insert_collection(&a, "c", "props", None).await;
        for asset in ["here", "elsewhere"] {
            sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES (?, 't')")
                .bind(asset)
                .execute(&a)
                .await
                .unwrap();
            sqlx::query("INSERT INTO collection_assets (collection_id, asset_id) VALUES ('c', ?)")
                .bind(asset)
                .execute(&a)
                .await
                .unwrap();
        }

        db.sync_open_libraries().await;
        db.sync_open_libraries().await;

        let tagged: Vec<String> = sqlx::query_scalar("SELECT asset_id FROM asset_tags")
            .fetch_all(&a)
            .await
            .unwrap();
        let collected: Vec<String> = sqlx::query_scalar("SELECT asset_id FROM collection_assets")
            .fetch_all(&a)
            .await
            .unwrap();
        assert_eq!(tagged, ["here"]);
        assert_eq!(collected, ["here"]);
    }

    #[tokio::test]
    async fn registering_a_library_mirrors_its_new_definitions_to_the_other_open_libraries() {
        let root = std::env::temp_dir().join(format!("opendam-new-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".opendam")).unwrap();
        let existing = connect_library_sqlite(&root.join(".opendam").join("library.sqlite"))
            .await
            .unwrap();
        insert_tag(&existing, "u", "brought along").await;
        existing.close().await;
        let a = memory_library_pool().await;
        let db = memory_database(vec![memory_library("a", a.clone())]).await;

        let result = db.register_library("new", "new", &root).await;
        std::fs::remove_dir_all(&root).unwrap();

        result.unwrap();
        assert_eq!(
            tag_name(&db.master_pool, "u").await.as_deref(),
            Some("brought along")
        );
        assert_eq!(tag_name(&a, "u").await.as_deref(), Some("brought along"));
    }

    #[tokio::test]
    async fn startup_mirrors_a_later_librarys_new_definitions_to_an_earlier_library() {
        let dir = std::env::temp_dir().join(format!("opendam-startup-{}", uuid::Uuid::new_v4()));
        let master = dir.join("master.sqlite");
        for id in ["a", "b"] {
            std::fs::create_dir_all(dir.join(id).join(".opendam")).unwrap();
        }
        let b = connect_library_sqlite(&dir.join("b/.opendam/library.sqlite"))
            .await
            .unwrap();
        insert_tag(&b, "u", "brought by b").await;
        insert_collection(&b, "c", "brought by b", None).await;
        b.close().await;
        let first_run = Database::new(master.to_str().unwrap()).await.unwrap();
        for id in ["a", "b"] {
            sqlx::query(
                "INSERT INTO libraries (id, name, root_paths, created_at) VALUES (?, ?, ?, 0)",
            )
            .bind(id)
            .bind(id)
            .bind(serde_json::json!([dir.join(id)]).to_string())
            .execute(&first_run.master_pool)
            .await
            .unwrap();
        }
        first_run.master_pool.close().await;

        let db = Database::new(master.to_str().unwrap()).await.unwrap();
        let a = db.get_library_handle("a").await.unwrap().pool;
        let mirrored = (tag_name(&a, "u").await, collection_name(&a, "c").await);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(
            mirrored,
            (Some("brought by b".into()), Some("brought by b".into()))
        );
    }

    async fn tag_description(pool: &Pool<Sqlite>, id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT description FROM tags WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    async fn set_description(pool: &Pool<Sqlite>, id: &str, description: &str) {
        sqlx::query("UPDATE tags SET description = ? WHERE id = ?")
            .bind(description)
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn opening_a_library_carries_tag_hints_master_wins() {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        let db = memory_database(vec![
            memory_library("a", a.clone()),
            memory_library("b", b.clone()),
        ])
        .await;
        insert_tag(&db.master_pool, "t", "rock").await;
        set_description(&db.master_pool, "t", "master hint").await;
        insert_tag(&a, "t", "rock").await;
        set_description(&a, "t", "stale hint").await;
        insert_tag(&a, "u", "moss").await;
        set_description(&a, "u", "hint only a had").await;

        db.sync_open_libraries().await;

        for pool in [&db.master_pool, &a, &b] {
            assert_eq!(
                tag_description(pool, "t").await.as_deref(),
                Some("master hint")
            );
            assert_eq!(
                tag_description(pool, "u").await.as_deref(),
                Some("hint only a had")
            );
        }
    }

    #[tokio::test]
    async fn a_master_db_made_before_run_notices_gains_the_column_and_keeps_its_runs() {
        let pool = memory_library_pool().await;
        sqlx::query(
            "CREATE TABLE tag_runs (id TEXT PRIMARY KEY, label TEXT NOT NULL,
                 provider TEXT NOT NULL, access TEXT, model TEXT, mode TEXT NOT NULL,
                 status TEXT NOT NULL, pause_reason TEXT, inline INTEGER NOT NULL DEFAULT 0,
                 options TEXT NOT NULL DEFAULT '{}', input_tokens INTEGER NOT NULL DEFAULT 0,
                 output_tokens INTEGER NOT NULL DEFAULT 0, cost_usd REAL NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL, finished_at INTEGER);
             INSERT INTO tag_runs (id, label, provider, mode, status, created_at)
             VALUES ('r', 'old run', 'anthropic', 'existing_only', 'done', 0);",
        )
        .execute(&pool)
        .await
        .unwrap();

        create_master_tables(&pool).await.unwrap();
        create_master_tables(&pool).await.unwrap();

        let run: (String, Option<String>) = sqlx::query_as("SELECT label, notice FROM tag_runs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(run, ("old run".to_string(), None));
    }

    #[tokio::test]
    async fn opening_databases_made_before_tag_hints_adds_the_column_and_keeps_their_tags() {
        let dir = std::env::temp_dir().join(format!("opendam-hints-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("lib/.opendam")).unwrap();
        let master = dir.join("master.sqlite");
        let old_tags = "CREATE TABLE tags (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                        color TEXT, parent_id TEXT REFERENCES tags(id) ON DELETE SET NULL);
                        INSERT INTO tags (id, name, color) VALUES ('t', 'rock', 'red');";
        for file in [master.clone(), dir.join("lib/.opendam/library.sqlite")] {
            let options = SqliteConnectOptions::new()
                .filename(&file)
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new()
                .connect_with(options)
                .await
                .unwrap();
            sqlx::query(old_tags).execute(&pool).await.unwrap();
            pool.close().await;
        }
        let first_run = Database::new(master.to_str().unwrap()).await.unwrap();
        sqlx::query(
            "INSERT INTO libraries (id, name, root_paths, created_at) VALUES ('lib', 'lib', ?, 0)",
        )
        .bind(serde_json::json!([dir.join("lib")]).to_string())
        .execute(&first_run.master_pool)
        .await
        .unwrap();
        first_run.master_pool.close().await;

        let db = Database::new(master.to_str().unwrap()).await.unwrap();
        let lib = db.get_library_handle("lib").await.unwrap().pool;
        let rows = [
            sqlx::query_as::<_, TagRow>("SELECT id, name, color, parent_id, description FROM tags")
                .fetch_all(&db.master_pool)
                .await,
            sqlx::query_as::<_, TagRow>("SELECT id, name, color, parent_id, description FROM tags")
                .fetch_all(&lib)
                .await,
        ];
        std::fs::remove_dir_all(&dir).unwrap();

        let expected: TagRow = (
            "t".into(),
            "rock".into(),
            Some("red".into()),
            None,
            String::new(),
        );
        for row in rows {
            assert_eq!(row.unwrap(), std::slice::from_ref(&expected));
        }
    }

    #[tokio::test]
    async fn every_library_open_has_the_name_and_size_sort_indexes_once() {
        let dir = std::env::temp_dir().join(format!("opendam-idx-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("library.sqlite");
        // A library from before the indexes existed.
        let old = connect_library_sqlite(&file).await.unwrap();
        sqlx::raw_sql(
            "DROP INDEX idx_assets_file_name_nocase;
             DROP INDEX idx_assets_size_bytes;",
        )
        .execute(&old)
        .await
        .unwrap();
        old.close().await;

        for _ in 0..2 {
            let pool = connect_library_sqlite(&file).await.unwrap();
            let indexes: Vec<String> = sqlx::query_scalar(
                "SELECT sql FROM sqlite_master
                 WHERE name IN ('idx_assets_file_name_nocase', 'idx_assets_size_bytes')
                 ORDER BY name",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(
                indexes,
                [
                    "CREATE INDEX idx_assets_file_name_nocase ON assets(file_name COLLATE NOCASE)",
                    "CREATE INDEX idx_assets_size_bytes   ON assets(size_bytes)",
                ]
            );
            pool.close().await;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn adding_the_tag_description_column_fails_loudly_unless_it_is_already_there() {
        let dir = std::env::temp_dir().join(format!("opendam-ro-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (old, new) = (dir.join("old.sqlite"), dir.join("new.sqlite"));
        for (file, schema) in [
            (
                &old,
                "CREATE TABLE tags (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE)",
            ),
            (&new, LIBRARY_SCHEMA),
        ] {
            let pool = SqlitePoolOptions::new()
                .connect_with(
                    SqliteConnectOptions::new()
                        .filename(file)
                        .create_if_missing(true),
                )
                .await
                .unwrap();
            sqlx::raw_sql(schema).execute(&pool).await.unwrap();
            pool.close().await;
        }
        // Read-only stands in for any ALTER failure, such as a share locked by another machine.
        let read_only = |file: &PathBuf| {
            SqlitePoolOptions::new()
                .connect_with(SqliteConnectOptions::new().filename(file).read_only(true))
        };

        let lacking = add_tag_description_column(&read_only(&old).await.unwrap()).await;
        let present = add_tag_description_column(&read_only(&new).await.unwrap()).await;
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(lacking.is_err());
        assert!(present.is_ok());
    }

    #[tokio::test]
    async fn adding_the_run_notice_column_fails_loudly_unless_it_is_already_there() {
        let dir = std::env::temp_dir().join(format!("opendam-ro-runs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (old, new) = (dir.join("old.sqlite"), dir.join("new.sqlite"));
        for (file, schema) in [
            (
                &old,
                "CREATE TABLE tag_runs (id TEXT PRIMARY KEY, status TEXT NOT NULL)",
            ),
            (
                &new,
                "CREATE TABLE tag_runs (id TEXT PRIMARY KEY, status TEXT NOT NULL, notice TEXT)",
            ),
        ] {
            let pool = SqlitePoolOptions::new()
                .connect_with(
                    SqliteConnectOptions::new()
                        .filename(file)
                        .create_if_missing(true),
                )
                .await
                .unwrap();
            sqlx::raw_sql(schema).execute(&pool).await.unwrap();
            pool.close().await;
        }
        // Read-only stands in for any ALTER failure, such as SQLITE_BUSY
        let read_only = |file: &PathBuf| {
            SqlitePoolOptions::new()
                .connect_with(SqliteConnectOptions::new().filename(file).read_only(true))
        };

        let lacking = add_run_columns(&read_only(&old).await.unwrap()).await;
        let present = add_run_columns(&read_only(&new).await.unwrap()).await;
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(lacking.is_err());
        assert!(present.is_ok());
    }

    #[tokio::test]
    async fn adding_the_preview_columns_to_a_library_made_before_them_keeps_its_rows() {
        let pool = memory_library_pool().await;
        insert_asset(&pool, "a1").await;
        sqlx::query(
            "ALTER TABLE assets DROP COLUMN preview_version;
             ALTER TABLE assets DROP COLUMN preview_textures;",
        )
        .execute(&pool)
        .await
        .unwrap();

        add_preview_columns(&pool).await.unwrap();
        add_preview_columns(&pool).await.unwrap();

        let row: (String, Option<i64>, Option<String>) =
            sqlx::query_as("SELECT id, preview_version, preview_textures FROM assets")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, ("a1".to_string(), None, None));
    }

    #[tokio::test]
    async fn adding_the_pack_origin_columns_to_a_library_made_before_them_keeps_its_overrides() {
        let pool = memory_library_pool().await;
        sqlx::query(
            "DROP TABLE pack_overrides;
             CREATE TABLE pack_overrides (
                 path TEXT PRIMARY KEY,
                 mark TEXT CHECK (mark IN ('pack', 'not_pack')),
                 name TEXT, license TEXT, source_url TEXT, cover_path TEXT,
                 updated_at INTEGER NOT NULL);
             INSERT INTO pack_overrides (path, mark, name, license, source_url, cover_path, updated_at)
                 VALUES ('Kit', 'pack', 'My kit', 'CC0', 'https://example.com', 'Kit/a.png', 1);",
        )
        .execute(&pool)
        .await
        .unwrap();

        add_pack_override_columns(&pool).await.unwrap();
        add_pack_override_columns(&pool).await.unwrap();

        let overrides = crate::structure::read_overrides(&pool).await.unwrap();
        assert_eq!(
            overrides.get("Kit"),
            Some(&crate::structure::PackOverride {
                mark: Some(crate::structure::PackMark::Pack),
                name: Some("My kit".into()),
                license: Some("CC0".into()),
                source_url: Some("https://example.com".into()),
                cover_path: Some("Kit/a.png".into()),
                origin: None,
                normal_convention: None,
            })
        );
        sqlx::query("UPDATE pack_overrides SET origin = 'unreal', normal_convention = 'directx'")
            .execute(&pool)
            .await
            .unwrap();
        let bad = sqlx::query("UPDATE pack_overrides SET origin = 'godot'")
            .execute(&pool)
            .await;
        assert!(bad.is_err(), "an unknown origin is refused");
        let kit = crate::structure::read_overrides(&pool).await.unwrap()["Kit"].clone();
        assert_eq!(
            (kit.origin, kit.normal_convention),
            (
                Some(crate::export::packs::Origin::Unreal),
                Some(crate::export::textures::Convention::DirectX)
            )
        );
    }

    #[tokio::test]
    async fn get_all_libraries_lists_libraries_in_id_order() {
        let mut libraries = Vec::new();
        for id in ["d", "b", "e", "a", "c"] {
            libraries.push(memory_library(id, memory_library_pool().await));
        }
        let db = memory_database(libraries).await;

        let ids: Vec<String> = db
            .get_all_libraries()
            .await
            .into_iter()
            .map(|l| l.id)
            .collect();
        assert_eq!(ids, ["a", "b", "c", "d", "e"]);
    }
}
