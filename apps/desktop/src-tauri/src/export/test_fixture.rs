//! The game export's fixture library, indexed in a temporary folder, for the export's tests.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::db::{insert_collection, memory_database, memory_library_pool, Database, LibraryHandle};
use crate::export::settings;

pub const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/game-export/library"
);

/// The fixture library copied into a temporary folder and indexed, an export root outside
/// it, and the root collection `DungeonGame`, both set.
pub struct Fixture {
    pub db: Database,
    pub library: LibraryHandle,
    pub export_root: PathBuf,
    pub dir: tempfile::TempDir,
}

pub async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("library");
    for entry in walkdir::WalkDir::new(FIXTURE) {
        let entry = entry.unwrap();
        let target = root.join(entry.path().strip_prefix(FIXTURE).unwrap());
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target).unwrap();
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
    let library = LibraryHandle {
        id: "fixture".into(),
        name: "Fixture".into(),
        root_path: root.clone(),
        pool: memory_library_pool().await,
    };
    let db = memory_database(vec![library.clone()]).await;
    sqlx::query(
        "INSERT INTO libraries (id, name, root_paths, created_at) VALUES ('fixture', 'Fixture', ?, 0)",
    )
    .bind(json!([root]).to_string())
    .execute(&db.master_pool)
    .await
    .unwrap();
    crate::watcher::index_paths(&db, "fixture", &[root.display().to_string()])
        .await
        .unwrap();

    let export_root = dir.path().join("export");
    std::fs::create_dir(&export_root).unwrap();
    insert_collection(&db.master_pool, "dg", "DungeonGame", None).await;
    settings::update(
        &db.master_pool,
        json!({"root": export_root, "rootCollection": "dg"}),
    )
    .await
    .unwrap();
    Fixture {
        db,
        library,
        export_root,
        dir,
    }
}

/// Opens, registers and indexes a second library `id` at `root`, beside the fixture.
pub async fn add_library(f: &Fixture, id: &str, root: &Path) -> LibraryHandle {
    let library = LibraryHandle {
        id: id.into(),
        name: id.into(),
        root_path: root.to_path_buf(),
        pool: memory_library_pool().await,
    };
    f.db.libraries
        .write()
        .await
        .insert(id.into(), library.clone());
    sqlx::query("INSERT INTO libraries (id, name, root_paths, created_at) VALUES (?, ?, ?, 0)")
        .bind(id)
        .bind(id)
        .bind(json!([root]).to_string())
        .execute(&f.db.master_pool)
        .await
        .unwrap();
    crate::watcher::index_paths(&f.db, id, &[root.display().to_string()])
        .await
        .unwrap();
    library
}

pub async fn id_of(pool: &Pool, path: &str) -> String {
    sqlx::query_scalar("SELECT id FROM assets WHERE file_path = ?")
        .bind(path)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{path} isn't indexed: {e}"))
}

pub type Pool = sqlx::Pool<sqlx::Sqlite>;

/// Adds collection `id` named `name` under `parent`, holding the fixture's files at `paths`.
pub async fn collection(f: &Fixture, id: &str, name: &str, parent: &str, paths: &[&str]) {
    insert_collection(&f.db.master_pool, id, name, Some(parent)).await;
    add_members(&f.library, id, paths).await;
}

/// Puts the files at `paths` of `library` in collection `id`.
pub async fn add_members(library: &LibraryHandle, id: &str, paths: &[&str]) {
    for path in paths {
        sqlx::query("INSERT INTO collection_assets (collection_id, asset_id) VALUES (?, ?)")
            .bind(id)
            .bind(id_of(&library.pool, path).await)
            .execute(&library.pool)
            .await
            .unwrap();
    }
}
