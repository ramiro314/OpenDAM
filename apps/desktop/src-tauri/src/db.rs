use anyhow::Result;
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

/// Backward compatibility: tags gained a description (the hint the tagging providers get).
/// Fails harmlessly when the column is already there.
async fn add_tag_description_column(pool: &Pool<Sqlite>) {
    let _ = sqlx::query("ALTER TABLE tags ADD COLUMN description TEXT NOT NULL DEFAULT ''")
        .execute(pool)
        .await;
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

    // Execute the per-library schema
    sqlx::query(include_str!("../migrations/002_library_schema.sql"))
        .execute(&pool)
        .await?;

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
    add_tag_description_column(&pool).await;

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

        // Ensure libraries table exists in master DB
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS libraries (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL,
                root_paths  TEXT NOT NULL,
                created_at  INTEGER NOT NULL,
                asset_count INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )
        .execute(&master_pool)
        .await?;

        // Also ensure library schema tables exist on master_pool as fallback
        let _ = sqlx::query(include_str!("../migrations/002_library_schema.sql"))
            .execute(&master_pool)
            .await;

        let _ = sqlx::query("ALTER TABLE assets ADD COLUMN is_favorite INTEGER NOT NULL DEFAULT 0")
            .execute(&master_pool)
            .await;
        let _ =
            sqlx::query("CREATE INDEX IF NOT EXISTS idx_assets_is_favorite ON assets(is_favorite)")
                .execute(&master_pool)
                .await;
        add_tag_description_column(&master_pool).await;

        let mut open_libraries = HashMap::new();

        // Load all registered libraries and open their per-library SQLite pools
        if let Ok(rows) = sqlx::query_as::<_, (String, String, String)>(
            "SELECT id, name, root_paths FROM libraries ORDER BY id",
        )
        .fetch_all(&master_pool)
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

                        let lib_db_file = opendam_dir.join("library.sqlite");
                        match connect_library_sqlite(&lib_db_file).await {
                            Ok(lib_pool) => {
                                open_libraries.insert(
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
                                    lib_db_file,
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }

        let db = Self {
            master_pool,
            libraries: Arc::new(RwLock::new(open_libraries)),
        };

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

        let lib_db_file = opendam_dir.join("library.sqlite");
        let lib_pool = connect_library_sqlite(&lib_db_file).await?;

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
    sqlx::query(include_str!("../migrations/002_library_schema.sql"))
        .execute(&pool)
        .await
        .unwrap();
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

/// A database over an in-memory master and the given open libraries, for tests.
#[cfg(test)]
pub(crate) async fn memory_database(libraries: Vec<LibraryHandle>) -> Database {
    Database {
        master_pool: memory_library_pool().await,
        libraries: Arc::new(RwLock::new(
            libraries.into_iter().map(|h| (h.id.clone(), h)).collect(),
        )),
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
