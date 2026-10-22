//! Game export settings, one row per field in the master DB's `settings` table (`export.<field>`,
//! JSON value), as plan C2 describes: the export root folder, the root collection whose children
//! are the export domains, and Blender's path. An unset field has no row.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Pool, Sqlite};
use std::path::{Path, PathBuf};

use super::blender;

const ROOT_KEY: &str = "export.root";
const ROOT_COLLECTION_KEY: &str = "export.rootCollection";
const BLENDER_PATH_KEY: &str = "export.blenderPath";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSettings {
    /// An absolute folder outside every library (`check_outside_libraries`).
    pub root: Option<String>,
    /// A top-level, non-smart collection's id. Its children are the export domains.
    pub root_collection: Option<String>,
    /// An absolute path to Blender. Unset means `blender` on `PATH` (`blender::gate`).
    pub blender_path: Option<String>,
}

/// The saved settings. A saved value that isn't a JSON string counts as unset.
pub async fn load(pool: &Pool<Sqlite>) -> sqlx::Result<ExportSettings> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM settings WHERE key IN (?, ?, ?)")
            .bind(ROOT_KEY)
            .bind(ROOT_COLLECTION_KEY)
            .bind(BLENDER_PATH_KEY)
            .fetch_all(pool)
            .await?;

    let mut settings = ExportSettings::default();
    for (key, raw) in rows {
        let Ok(value) = serde_json::from_str::<String>(&raw) else {
            tracing::warn!("Ignoring the saved setting {key}: not a JSON string");
            continue;
        };
        match key.as_str() {
            ROOT_KEY => settings.root = Some(value),
            ROOT_COLLECTION_KEY => settings.root_collection = Some(value),
            _ => settings.blender_path = Some(value),
        }
    }
    Ok(settings)
}

/// Applies `patch`, such as `{"root": "/home/me/export"}` or `{"rootCollection": null}`, where
/// null clears a field, and saves the fields it names in one transaction. An unknown field, a
/// wrong type, an export root that isn't an absolute folder outside every library, a root
/// collection that isn't a top-level, non-smart collection, or a Blender path that isn't absolute
/// saves nothing. Blender's path isn't required to exist: Settings shows what it finds there.
pub async fn update(pool: &Pool<Sqlite>, patch: Value) -> Result<ExportSettings, String> {
    let Value::Object(fields) = patch else {
        return Err("Export settings patch must be an object".into());
    };
    let mut changed = Vec::new();
    for (field, value) in fields {
        let key = match field.as_str() {
            "root" => ROOT_KEY,
            "rootCollection" => ROOT_COLLECTION_KEY,
            "blenderPath" => BLENDER_PATH_KEY,
            _ => return Err(format!("Unknown setting export.{field}")),
        };
        let value = match value {
            Value::Null => None,
            // Blender's path is saved trimmed, as Settings checks it (`blender::check_path`)
            Value::String(s) if key == BLENDER_PATH_KEY => Some(s.trim().to_string()),
            Value::String(s) => Some(s),
            _ => return Err(format!("export.{field} must be a string or null")),
        }
        .filter(|s| !s.trim().is_empty());
        if let Some(value) = &value {
            match key {
                ROOT_KEY => check_root(pool, value).await?,
                ROOT_COLLECTION_KEY => check_root_collection(pool, value).await?,
                _ => blender::check_path(value)?,
            }
        }
        changed.push((key, value));
    }

    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    for (key, value) in &changed {
        match value {
            Some(value) => sqlx::query(
                "INSERT INTO settings (key, value) VALUES (?, ?)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(key)
            .bind(Value::String(value.clone()).to_string()),
            None => sqlx::query("DELETE FROM settings WHERE key = ?").bind(key),
        }
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    load(pool).await.map_err(|e| e.to_string())
}

/// Refuses an export root that isn't an absolute folder outside every library: on save, and at
/// each export check (plan C2).
pub async fn check_root(pool: &Pool<Sqlite>, root: &str) -> Result<(), String> {
    let path = PathBuf::from(root);
    if !path.is_absolute() {
        return Err(format!("The export root {root} is not an absolute path"));
    }
    if !path.is_dir() {
        return Err(format!("The export root {root} is not a folder"));
    }
    let libraries = library_roots(pool).await.map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || check_outside_libraries(&path, &libraries))
        .await
        .map_err(|e| e.to_string())?
}

async fn check_root_collection(pool: &Pool<Sqlite>, id: &str) -> Result<(), String> {
    let row: Option<(String, Option<String>, bool)> =
        sqlx::query_as("SELECT name, parent_id, is_smart FROM collections WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?;
    match row {
        None => Err(format!("No collection has the id {id}")),
        Some((name, Some(_), _)) => Err(format!(
            "The collection {name} is inside another one: the root collection must be top-level"
        )),
        Some((name, _, true)) => Err(format!(
            "The collection {name} is smart: the root collection must be an ordinary one"
        )),
        Some(_) => Ok(()),
    }
}

/// Every registered library's name and roots, open or offline, from the master DB.
pub async fn library_roots(pool: &Pool<Sqlite>) -> sqlx::Result<Vec<(String, PathBuf)>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT name, root_paths FROM libraries ORDER BY id")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .flat_map(|(name, json)| {
            let roots: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
            roots
                .into_iter()
                .map(move |r| (name.clone(), PathBuf::from(r)))
        })
        .collect())
}

/// Refuses an export root that equals a library root, lies inside one, or contains one (plan C2,
/// AC15), so the watcher never indexes exported files as new assets. Both sides are compared
/// after resolving symlinks (`resolve`). `libraries` holds each library's name and root, as
/// `library_roots` reads them. This touches the file system, so async callers run it on a
/// blocking thread.
pub fn check_outside_libraries(root: &Path, libraries: &[(String, PathBuf)]) -> Result<(), String> {
    let export = resolve(root);
    for (name, library_root) in libraries {
        let library = resolve(library_root);
        let shown = root.display();
        if export == library {
            return Err(format!(
                "The export root {shown} is the folder of the library {name}"
            ));
        }
        if export.starts_with(&library) {
            return Err(format!(
                "The export root {shown} is inside the library {name}, which would index the exported files"
            ));
        }
        if library.starts_with(&export) {
            return Err(format!(
                "The export root {shown} holds the library {name}, which would index the exported files"
            ));
        }
    }
    Ok(())
}

/// `path` with every symlink resolved: the canonical form of its deepest existing ancestor, with
/// the rest appended as written. So a folder an export has yet to create, or an offline
/// library's root, still compares by where it would be.
fn resolve(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(existing) {
            return rest
                .iter()
                .rev()
                .fold(canonical, |path, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{insert_collection, memory_database};
    use serde_json::json;

    async fn saved_rows(pool: &Pool<Sqlite>) -> Vec<(String, String)> {
        sqlx::query_as("SELECT key, value FROM settings ORDER BY key")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn register_library(pool: &Pool<Sqlite>, id: &str, roots: &[&Path]) {
        let roots: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
        sqlx::query("INSERT INTO libraries (id, name, root_paths, created_at) VALUES (?, ?, ?, 0)")
            .bind(id)
            .bind(id)
            .bind(serde_json::to_string(&roots).unwrap())
            .execute(pool)
            .await
            .unwrap();
    }

    fn libs(roots: &[&Path]) -> Vec<(String, PathBuf)> {
        roots
            .iter()
            .enumerate()
            .map(|(i, r)| (format!("lib{i}"), r.to_path_buf()))
            .collect()
    }

    #[tokio::test]
    async fn an_empty_table_leaves_every_setting_unset() {
        let db = memory_database(vec![]).await;

        assert_eq!(
            load(&db.master_pool).await.unwrap(),
            ExportSettings::default()
        );
    }

    #[tokio::test]
    async fn settings_round_trip_through_the_master_db() {
        let db = memory_database(vec![]).await;
        let pool = &db.master_pool;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().display().to_string();
        insert_collection(pool, "dg", "DungeonGame", None).await;

        let saved = update(
            pool,
            json!({"root": root, "rootCollection": "dg", "blenderPath": "/opt/blender/blender"}),
        )
        .await
        .unwrap();

        assert_eq!(load(pool).await.unwrap(), saved);
        assert_eq!(saved.root.as_deref(), Some(root.as_str()));
        assert_eq!(saved.root_collection.as_deref(), Some("dg"));
        assert_eq!(saved.blender_path.as_deref(), Some("/opt/blender/blender"));
        assert_eq!(
            saved_rows(pool).await,
            [
                (
                    "export.blenderPath".to_string(),
                    "\"/opt/blender/blender\"".to_string()
                ),
                ("export.root".to_string(), json!(root).to_string()),
                ("export.rootCollection".to_string(), "\"dg\"".to_string()),
            ]
        );

        // Blender's path is saved trimmed
        let trimmed = update(pool, json!({"blenderPath": " /opt/blender/blender "}))
            .await
            .unwrap();
        assert_eq!(
            trimmed.blender_path.as_deref(),
            Some("/opt/blender/blender")
        );

        // A patch saves only what it names, and null or an empty string clears a field
        let cleared = update(pool, json!({"root": null, "blenderPath": " "}))
            .await
            .unwrap();
        assert_eq!(cleared.root, None);
        assert_eq!(cleared.blender_path, None);
        assert_eq!(cleared.root_collection.as_deref(), Some("dg"));
        assert_eq!(saved_rows(pool).await.len(), 1);
    }

    #[tokio::test]
    async fn an_invalid_patch_is_an_error_and_saves_nothing() {
        let db = memory_database(vec![]).await;
        let pool = &db.master_pool;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        std::fs::write(&file, "").unwrap();
        insert_collection(pool, "dg", "DungeonGame", None).await;
        insert_collection(pool, "dungeon", "Dungeon", Some("dg")).await;
        insert_collection(pool, "smart", "Recent", None).await;
        sqlx::query("UPDATE collections SET is_smart = 1 WHERE id = 'smart'")
            .execute(pool)
            .await
            .unwrap();

        for patch in [
            json!({"root": "relative/export"}),
            json!({"root": dir.path().join("missing")}),
            json!({"root": file}),
            json!({"root": 7}),
            json!({"rootCollection": "nope"}),
            json!({"rootCollection": "dungeon"}),
            json!({"rootCollection": "smart"}),
            json!({"rootCollection": "dg", "blender": "/usr/bin/blender"}),
            json!({"blenderPath": "blender"}),
            json!({"blenderPath": "bin/blender"}),
            json!({"rootCollection": "dg", "blenderPath": 5}),
            json!({"root": dir.path(), "rootCollection": "smart"}),
            json!(["not", "an", "object"]),
        ] {
            assert!(update(pool, patch.clone()).await.is_err(), "{patch}");
        }
        assert!(saved_rows(pool).await.is_empty());
    }

    #[tokio::test]
    async fn an_export_root_in_a_library_is_refused_when_set() {
        let db = memory_database(vec![]).await;
        let pool = &db.master_pool;
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        let inside = library.join("export");
        std::fs::create_dir_all(&inside).unwrap();
        register_library(pool, "lib", &[&library]).await;

        let error = update(pool, json!({"root": inside})).await.unwrap_err();

        assert!(error.contains("inside the library lib"), "{error}");
        assert!(saved_rows(pool).await.is_empty());
    }

    #[test]
    fn an_export_root_equal_to_a_library_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();

        let error = check_outside_libraries(dir.path(), &libs(&[dir.path()])).unwrap_err();

        assert!(
            error.contains("is the folder of the library lib0"),
            "{error}"
        );
    }

    #[test]
    fn an_export_root_inside_a_library_is_refused_even_before_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        std::fs::create_dir(&library).unwrap();

        for root in [library.join("export"), library.join("not/made/yet")] {
            let error = check_outside_libraries(&root, &libs(&[&library])).unwrap_err();
            assert!(error.contains("inside the library"), "{error}");
        }
    }

    #[test]
    fn an_export_root_holding_a_library_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("assets/library");
        std::fs::create_dir_all(&library).unwrap();

        let error = check_outside_libraries(dir.path(), &libs(&[&library])).unwrap_err();

        assert!(error.contains("holds the library"), "{error}");
    }

    #[test]
    fn symlinks_are_resolved_on_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        std::fs::create_dir_all(library.join("models")).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();

        // An export root reached through a link that points into the library
        let link_in = dir.path().join("link-in");
        std::os::unix::fs::symlink(library.join("models"), &link_in).unwrap();
        let error = check_outside_libraries(&link_in, &libs(&[&library])).unwrap_err();
        assert!(error.contains("inside the library"), "{error}");

        // A library registered through a link to the export root itself
        let link_lib = dir.path().join("link-lib");
        std::os::unix::fs::symlink(&outside, &link_lib).unwrap();
        let error = check_outside_libraries(&outside, &libs(&[&link_lib])).unwrap_err();
        assert!(error.contains("is the folder of the library"), "{error}");
    }

    #[test]
    fn a_sibling_folder_with_the_same_prefix_is_outside() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("library");
        let export = dir.path().join("library-export");
        std::fs::create_dir(&library).unwrap();
        std::fs::create_dir(&export).unwrap();

        assert_eq!(
            check_outside_libraries(&export, &libs(&[&library, Path::new("/mnt/offline")])),
            Ok(())
        );
    }
}
