use chrono::Utc;
use serde_json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, Runtime, State};
use uuid::Uuid;

use crate::autotag::engine::Engine;
use crate::db::Database;
use crate::models::{AddLibraryPayload, Library};
use crate::watcher;
use crate::AppState;

type Result<T> = std::result::Result<T, String>;

/// Lets the webview load files under a library's roots through the asset protocol, whose static
/// scope in `tauri.conf.json` covers only app data.
pub fn allow_library_roots<R: Runtime>(app: &AppHandle<R>, root_paths: &[String]) {
    let scope = app.asset_protocol_scope();
    for root in root_paths {
        if let Err(e) = scope.allow_directory(root, true) {
            tracing::error!("Failed to allow asset access to {}: {}", root, e);
        }
    }
}

/// Refuses roots the user didn't pick. The native folder dialog adds the folder it returns to the
/// asset protocol scope, so a root outside the scope came from a script, and allowing it would let
/// that script widen the scope to any path, `/` included.
fn check_roots_were_picked<R: Runtime>(app: &AppHandle<R>, root_paths: &[String]) -> Result<()> {
    let scope = app.asset_protocol_scope();
    match root_paths.iter().find(|root| !scope.is_allowed(root)) {
        Some(root) => Err(format!("{root} was not chosen in the folder picker")),
        None => Ok(()),
    }
}

#[tauri::command]
pub async fn add_library(
    payload: AddLibraryPayload,
    state: State<'_, Arc<AppState>>,
) -> Result<Library> {
    check_roots_were_picked(&state.app_handle, &payload.root_paths)?;
    let library = open_library(
        &state.db,
        &state.autotag,
        &payload.name,
        &payload.root_paths,
    )
    .await?;
    allow_library_roots(&state.app_handle, &library.root_paths);

    // Kick off background NAS-efficient indexing
    let state_clone = Arc::clone(&state);
    let paths = library.root_paths.clone();
    let lib_id = library.id.clone();
    tokio::spawn(async move {
        if let Err(e) = watcher::index_paths(&state_clone.db, &lib_id, &paths).await {
            tracing::error!("Failed to index library {}: {}", lib_id, e);
        } else {
            let _ = state_clone.app_handle.emit("library:updated", ());
        }
    });

    Ok(library)
}

/// Opens the folder at `root_paths` as a library: the registered library it already is when
/// that one is offline (its folder was missing when the app started, a NAS not mounted yet),
/// else a new library. A library reopened this way keeps its id, name and roots, and the tag
/// writes auto-tag runs left waiting for it are replayed. A new library has nothing waiting.
pub(crate) async fn open_library(
    db: &Database,
    autotag: &Engine,
    name: &str,
    root_paths: &[String],
) -> Result<Library> {
    let first_path = root_paths.first().ok_or("No root path provided")?;
    let root_path = PathBuf::from(first_path);

    if let Some(library) = offline_library_at(db, &root_path).await? {
        // Register library in memory & open its .opendam/library.sqlite with NAS pragmas
        db.register_library(&library.id, &library.name, &root_path)
            .await
            .map_err(|e| e.to_string())?;
        autotag.library_opened(&library.id).await;
        tracing::info!("Reopened offline library {} at {first_path}", library.id);
        return Ok(library);
    }

    let id = Uuid::new_v4().to_string();
    let now = Utc::now().timestamp_millis();
    let root_paths_json = serde_json::to_string(root_paths).map_err(|e| e.to_string())?;
    db.register_library(&id, name, &root_path)
        .await
        .map_err(|e| e.to_string())?;

    // Record in master database
    sqlx::query(
        "INSERT INTO libraries (id, name, root_paths, created_at, asset_count) VALUES (?, ?, ?, ?, 0)"
    )
    .bind(&id)
    .bind(name)
    .bind(&root_paths_json)
    .bind(now)
    .execute(&db.master_pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(Library {
        id,
        name: name.to_string(),
        root_paths: root_paths.to_vec(),
        created_at: now,
        asset_count: 0,
    })
}

/// The registered library whose first root is `root_path`, if it isn't open.
async fn offline_library_at(db: &Database, root_path: &Path) -> Result<Option<Library>> {
    let rows = sqlx::query_as::<_, (String, String, String, i64, i64)>(
        "SELECT id, name, root_paths, created_at, asset_count FROM libraries ORDER BY created_at, id",
    )
    .fetch_all(&db.master_pool)
    .await
    .map_err(|e| e.to_string())?;
    for (id, name, root_paths_json, created_at, asset_count) in rows {
        let root_paths: Vec<String> = serde_json::from_str(&root_paths_json).unwrap_or_default();
        let same_folder = root_paths
            .first()
            .is_some_and(|first| Path::new(first) == root_path);
        if same_folder && db.get_library_handle(&id).await.is_none() {
            return Ok(Some(Library {
                id,
                name,
                root_paths,
                created_at,
                asset_count,
            }));
        }
    }
    Ok(None)
}

#[tauri::command]
pub async fn list_libraries(state: State<'_, Arc<AppState>>) -> Result<Vec<Library>> {
    let rows = sqlx::query_as::<_, (String, String, String, i64, i64)>(
        "SELECT id, name, root_paths, created_at, asset_count FROM libraries ORDER BY created_at ASC"
    )
    .fetch_all(&state.db.master_pool)
    .await
    .map_err(|e| e.to_string())?;

    rows.into_iter()
        .map(|(id, name, root_paths_json, created_at, asset_count)| {
            let root_paths: Vec<String> =
                serde_json::from_str(&root_paths_json).unwrap_or_default();
            Ok(Library {
                id,
                name,
                root_paths,
                created_at,
                asset_count,
            })
        })
        .collect()
}

#[tauri::command]
pub async fn remove_library(id: String, state: State<'_, Arc<AppState>>) -> Result<()> {
    let _ = state.db.unregister_library(&id).await;

    sqlx::query("DELETE FROM libraries WHERE id = ?")
        .bind(&id)
        .execute(&state.db.master_pool)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

#[tauri::command]
pub async fn refresh_library(id: String, state: State<'_, Arc<AppState>>) -> Result<()> {
    let row = sqlx::query_as::<_, (String,)>("SELECT root_paths FROM libraries WHERE id = ?")
        .bind(&id)
        .fetch_optional(&state.db.master_pool)
        .await
        .map_err(|e| e.to_string())?;

    if let Some((root_paths_json,)) = row {
        let paths: Vec<String> = serde_json::from_str(&root_paths_json).unwrap_or_default();
        watcher::index_paths(&state.db, &id, &paths)
            .await
            .map_err(|e| e.to_string())?;
        let _ = state.app_handle.emit("library:updated", ());
    }
    Ok(())
}

#[tauri::command]
pub async fn refresh_all_libraries(state: State<'_, Arc<AppState>>) -> Result<()> {
    let rows = sqlx::query_as::<_, (String, String)>("SELECT id, root_paths FROM libraries")
        .fetch_all(&state.db.master_pool)
        .await
        .map_err(|e| e.to_string())?;

    for (id, root_paths_json) in rows {
        let paths: Vec<String> = serde_json::from_str(&root_paths_json).unwrap_or_default();
        let _ = watcher::index_paths(&state.db, &id, &paths).await;
    }

    let _ = state.app_handle.emit("library:updated", ());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_library_sqlite, insert_asset, insert_tag, memory_database};

    fn engine_over(db: &Arc<Database>) -> Arc<Engine> {
        Engine::new(db.clone(), Vec::new(), Arc::new(|_| {}))
    }

    async fn library_rows(db: &Database) -> Vec<(String, String)> {
        sqlx::query_as("SELECT id, name FROM libraries ORDER BY id")
            .fetch_all(&db.master_pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn adding_an_offline_librarys_folder_reopens_it_and_replays_its_waiting_tags() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("nas-library");
        std::fs::create_dir_all(root.join(".opendam")).unwrap();
        let lib = connect_library_sqlite(&root.join(".opendam/library.sqlite"))
            .await
            .unwrap();
        insert_asset(&lib, "a1").await;
        lib.close().await;
        let root = root.to_string_lossy().into_owned();
        // Registered, but its folder was missing when the app started, so it isn't open.
        let db = Arc::new(memory_database(Vec::new()).await);
        let master = &db.master_pool;
        sqlx::query(
            "INSERT INTO libraries (id, name, root_paths, created_at, asset_count)
             VALUES ('nas', 'NAS', ?, 7, 1)",
        )
        .bind(serde_json::to_string(&[&root]).unwrap())
        .execute(master)
        .await
        .unwrap();
        insert_tag(master, "t1", "Rock").await;
        sqlx::query(
            "INSERT INTO tag_runs (id, label, provider, mode, status, created_at)
             VALUES ('run', 'run', 'fake', 'existing_only', 'done', 0);
             INSERT INTO tag_run_applied (run_id, asset_id, library_id, tag_id)
             VALUES ('run', 'a1', 'nas', 't1');",
        )
        .execute(master)
        .await
        .unwrap();

        let library = open_library(
            &db,
            &engine_over(&db),
            "Picked again",
            std::slice::from_ref(&root),
        )
        .await
        .unwrap();

        assert_eq!(
            (
                library.id.as_str(),
                library.name.as_str(),
                library.created_at
            ),
            ("nas", "NAS", 7),
            "the registered library, not a new one"
        );
        assert_eq!(library_rows(&db).await, [("nas".into(), "NAS".into())]);
        let lib = db.get_library_handle("nas").await.unwrap().pool;
        let tags: Vec<(String, String)> = sqlx::query_as("SELECT asset_id, tag_id FROM asset_tags")
            .fetch_all(&lib)
            .await
            .unwrap();
        assert_eq!(
            tags,
            [("a1".into(), "t1".into())],
            "the waiting write landed"
        );
        let state: String = sqlx::query_scalar("SELECT state FROM tag_run_applied")
            .fetch_one(master)
            .await
            .unwrap();
        assert_eq!(state, "applied");
    }

    #[tokio::test]
    async fn adding_a_new_folder_registers_a_new_library() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        let db = Arc::new(memory_database(Vec::new()).await);

        let library = open_library(
            &db,
            &engine_over(&db),
            "Sprites",
            std::slice::from_ref(&root),
        )
        .await
        .unwrap();

        assert_eq!(library.name, "Sprites");
        assert_eq!(library.root_paths, [root]);
        assert_eq!(
            library_rows(&db).await,
            [(library.id.clone(), "Sprites".into())]
        );
        assert!(db.get_library_handle(&library.id).await.is_some());
    }

    #[tokio::test]
    async fn adding_an_open_librarys_folder_again_never_replays_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        let db = Arc::new(memory_database(Vec::new()).await);
        let engine = engine_over(&db);
        let open = open_library(&db, &engine, "Sprites", std::slice::from_ref(&root))
            .await
            .unwrap();
        let lib = db.get_library_handle(&open.id).await.unwrap().pool;
        insert_asset(&lib, "a1").await;
        insert_tag(&db.master_pool, "t1", "Rock").await;
        // An item of a running run has recorded its intent and is about to write the library.
        sqlx::query(
            "INSERT INTO tag_runs (id, label, provider, mode, status, created_at)
             VALUES ('run', 'run', 'fake', 'existing_only', 'running', 0)",
        )
        .execute(&db.master_pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tag_run_applied (run_id, asset_id, library_id, tag_id)
             VALUES ('run', 'a1', ?, 't1')",
        )
        .bind(&open.id)
        .execute(&db.master_pool)
        .await
        .unwrap();

        open_library(&db, &engine, "Sprites", &[root])
            .await
            .unwrap();

        let tagged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM asset_tags")
            .fetch_one(&lib)
            .await
            .unwrap();
        let state: String = sqlx::query_scalar("SELECT state FROM tag_run_applied")
            .fetch_one(&db.master_pool)
            .await
            .unwrap();
        assert_eq!(
            (tagged, state.as_str()),
            (0, "pending"),
            "the in-flight item's row is left to the item"
        );
    }

    #[test]
    fn only_roots_the_folder_dialog_allowed_can_become_libraries() {
        let app = tauri::test::mock_app();
        let picked = std::env::temp_dir().join(format!("opendam-picked-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&picked).unwrap();
        // What the dialog plugin does with the folder it returns
        app.asset_protocol_scope()
            .allow_directory(&picked, true)
            .unwrap();
        let picked = picked.to_string_lossy().into_owned();
        let home = std::env::var("HOME").unwrap();

        assert!(check_roots_were_picked(app.handle(), std::slice::from_ref(&picked)).is_ok());
        assert!(check_roots_were_picked(app.handle(), &[picked.clone(), "/".into()]).is_err());
        assert!(check_roots_were_picked(app.handle(), &[home]).is_err());
        assert!(check_roots_were_picked(app.handle(), &[format!("{picked}/../..")]).is_err());

        std::fs::remove_dir_all(&picked).unwrap();
    }
}
