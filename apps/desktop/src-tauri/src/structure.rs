//! The structure pass: a library's folder tree, derived from the paths it has indexed
//! (docs/plans/smarter-big-collections/plan.md, "The structure pass"). `derive` is a pure function
//! over the paths, and `apply` writes only the folder rows that differ from it. Every index run
//! ends with the pass (`watcher::index_paths`), and so does every command that changes the paths,
//! through `rebuild_library`. Nothing else writes `folders`.

use sqlx::{Pool, Sqlite};
use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use crate::db::{Database, LibraryHandle};

/// Rows per write transaction, as in `watcher.rs`.
const BATCH: usize = 200;

/// One folder as the pass derives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// `None` for the root
    pub parent_path: Option<String>,
    pub name: String,
    /// Files below the folder, at any depth. Every file counts as one item.
    pub item_count: i64,
}

/// Every folder by its path relative to the library root. The root is `""`.
pub type Structure = BTreeMap<String, Folder>;

/// The folders of a library holding the files at `paths` (relative, `/`-separated): the root,
/// plus every folder with at least one file below it, each counting the files below it.
pub fn derive<'a>(paths: impl IntoIterator<Item = &'a str>) -> Structure {
    let mut folders = Structure::new();
    folders.insert(
        String::new(),
        Folder {
            parent_path: None,
            name: String::new(),
            item_count: 0,
        },
    );
    for path in paths {
        folders.get_mut("").expect("the root").item_count += 1;
        for (end, _) in path.match_indices('/') {
            let dir = &path[..end];
            if dir.is_empty() {
                continue;
            }
            match folders.get_mut(dir) {
                Some(folder) => folder.item_count += 1,
                None => {
                    let (parent, name) = dir.rsplit_once('/').unwrap_or(("", dir));
                    folders.insert(
                        dir.to_string(),
                        Folder {
                            parent_path: Some(parent.to_string()),
                            name: name.to_string(),
                            item_count: 1,
                        },
                    );
                }
            }
        }
    }
    folders
}

/// Makes `folders` hold exactly `structure`, writing only the rows that differ, in transactions
/// of `BATCH` rows. Returns how many rows it inserted, updated or deleted.
pub async fn apply(pool: &Pool<Sqlite>, structure: &Structure) -> sqlx::Result<usize> {
    let rows: Vec<(String, Option<String>, String, i64)> =
        sqlx::query_as("SELECT path, parent_path, name, item_count FROM folders")
            .fetch_all(pool)
            .await?;
    let current: HashMap<String, Folder> = rows
        .into_iter()
        .map(|(path, parent_path, name, item_count)| {
            let folder = Folder {
                parent_path,
                name,
                item_count,
            };
            (path, folder)
        })
        .collect();
    let changed: Vec<(&String, &Folder)> = structure
        .iter()
        .filter(|(path, folder)| current.get(*path) != Some(folder))
        .collect();
    let gone: Vec<&String> = current
        .keys()
        .filter(|path| !structure.contains_key(*path))
        .collect();

    for chunk in changed.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for (path, folder) in chunk {
            // An upsert, so a later step's pack columns survive a change in the counts.
            sqlx::query(
                "INSERT INTO folders (path, parent_path, name, item_count) VALUES (?, ?, ?, ?)
                 ON CONFLICT(path) DO UPDATE SET parent_path = excluded.parent_path,
                     name = excluded.name, item_count = excluded.item_count",
            )
            .bind(path)
            .bind(&folder.parent_path)
            .bind(&folder.name)
            .bind(folder.item_count)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }
    for chunk in gone.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for path in chunk {
            sqlx::query("DELETE FROM folders WHERE path = ?")
                .bind(path)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
    Ok(changed.len() + gone.len())
}

/// Runs the pass on the library DB at `pool`: reads every indexed path, derives the folders and
/// applies them. Returns the rows written. The caller holds the library's `index_lock`.
pub async fn rebuild(pool: &Pool<Sqlite>) -> sqlx::Result<usize> {
    let started = Instant::now();
    let paths: Vec<String> = sqlx::query_scalar("SELECT file_path FROM assets")
        .fetch_all(pool)
        .await?;
    let structure = derive(paths.iter().map(String::as_str));
    let written = apply(pool, &structure).await?;
    tracing::info!(
        target: "structure",
        "Structure pass: {} files, {} folders, {} rows written in {:?}",
        paths.len(),
        structure.len(),
        written,
        started.elapsed()
    );
    Ok(written)
}

/// Reruns the pass for `library` once any index run on it has finished, for a command that
/// changed its paths. A failed pass is logged, not returned: the change itself went through,
/// and the next index run derives the folders again.
pub async fn rebuild_library(db: &Database, library: &LibraryHandle) {
    let lock = db.index_lock(&library.id);
    let _guard = lock.lock().await;
    if let Err(e) = rebuild(&library.pool).await {
        tracing::error!(target: "structure", "Structure pass failed for library {}: {e}", library.id);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::memory_library_pool;

    fn folder(parent_path: Option<&str>, name: &str, item_count: i64) -> Folder {
        Folder {
            parent_path: parent_path.map(str::to_string),
            name: name.to_string(),
            item_count,
        }
    }

    /// A Kenney-like layout: two kits in one category, one sharing the other's name as a prefix,
    /// a file at the root and a file directly in a category.
    const KENNEY: [&str; 7] = [
        "3D assets/Platformer Kit/License.txt",
        "3D assets/Platformer Kit/Models/GLB format/block.glb",
        "3D assets/Platformer Kit/Models/OBJ format/block.obj",
        "3D assets/Platformer Kit 2/crate.glb",
        "3D assets/index.md",
        "2D assets/UI Pack/PNG/button.png",
        "Readme.txt",
    ];

    #[test]
    fn every_folder_counts_the_files_below_it_and_the_root_counts_them_all() {
        let structure = derive(KENNEY);
        let expected = Structure::from(
            [
                ("", folder(None, "", 7)),
                ("2D assets", folder(Some(""), "2D assets", 1)),
                ("2D assets/UI Pack", folder(Some("2D assets"), "UI Pack", 1)),
                (
                    "2D assets/UI Pack/PNG",
                    folder(Some("2D assets/UI Pack"), "PNG", 1),
                ),
                ("3D assets", folder(Some(""), "3D assets", 5)),
                (
                    "3D assets/Platformer Kit",
                    folder(Some("3D assets"), "Platformer Kit", 3),
                ),
                (
                    "3D assets/Platformer Kit 2",
                    folder(Some("3D assets"), "Platformer Kit 2", 1),
                ),
                (
                    "3D assets/Platformer Kit/Models",
                    folder(Some("3D assets/Platformer Kit"), "Models", 2),
                ),
                (
                    "3D assets/Platformer Kit/Models/GLB format",
                    folder(Some("3D assets/Platformer Kit/Models"), "GLB format", 1),
                ),
                (
                    "3D assets/Platformer Kit/Models/OBJ format",
                    folder(Some("3D assets/Platformer Kit/Models"), "OBJ format", 1),
                ),
            ]
            .map(|(path, f)| (path.to_string(), f)),
        );
        assert_eq!(structure, expected);
    }

    #[test]
    fn a_library_with_no_files_has_only_its_root() {
        assert_eq!(
            derive([]),
            Structure::from([(String::new(), folder(None, "", 0))])
        );
    }

    #[test]
    fn files_only_at_the_root_make_no_folders() {
        assert_eq!(
            derive(["a.png", "b.glb"]),
            Structure::from([(String::new(), folder(None, "", 2))])
        );
    }

    #[test]
    fn the_order_of_the_paths_does_not_matter() {
        let mut reversed = KENNEY;
        reversed.reverse();
        assert_eq!(derive(reversed), derive(KENNEY));
    }

    /// Every `folders` row, in path order, for comparing a library with a fresh pass.
    pub(crate) async fn folder_rows(
        pool: &Pool<Sqlite>,
    ) -> Vec<(String, Option<String>, String, i64)> {
        sqlx::query_as("SELECT path, parent_path, name, item_count FROM folders ORDER BY path")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// The `folders` rows a fresh library holding `paths` gets from its first pass.
    pub(crate) async fn fresh_pass(paths: &[&str]) -> Vec<(String, Option<String>, String, i64)> {
        let pool = memory_library_pool().await;
        for path in paths {
            insert_path(&pool, path).await;
        }
        rebuild(&pool).await.unwrap();
        folder_rows(&pool).await
    }

    async fn insert_path(pool: &Pool<Sqlite>, path: &str) {
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
             VALUES (?, ?, ?, 'glb', '3d_model', 1, 0, 0)",
        )
        .bind(path)
        .bind(path)
        .bind(path.rsplit('/').next().unwrap())
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_rerun_with_no_changes_writes_nothing() {
        let pool = memory_library_pool().await;
        for path in KENNEY {
            insert_path(&pool, path).await;
        }
        assert_eq!(rebuild(&pool).await.unwrap(), 10);
        assert_eq!(rebuild(&pool).await.unwrap(), 0);
        assert_eq!(folder_rows(&pool).await, fresh_pass(&KENNEY).await);
    }

    #[tokio::test]
    async fn a_removed_file_rewrites_only_its_ancestors_and_the_folders_it_empties() {
        let pool = memory_library_pool().await;
        for path in KENNEY {
            insert_path(&pool, path).await;
        }
        rebuild(&pool).await.unwrap();
        sqlx::query("DELETE FROM assets WHERE file_path = '2D assets/UI Pack/PNG/button.png'")
            .execute(&pool)
            .await
            .unwrap();

        // The root's count changes, and the three folders it emptied go.
        assert_eq!(rebuild(&pool).await.unwrap(), 4);
        let rest: Vec<&str> = KENNEY
            .into_iter()
            .filter(|p| !p.starts_with("2D assets/"))
            .collect();
        assert_eq!(folder_rows(&pool).await, fresh_pass(&rest).await);
    }

    #[tokio::test]
    async fn a_count_change_keeps_the_pack_columns() {
        let pool = memory_library_pool().await;
        for path in KENNEY {
            insert_path(&pool, path).await;
        }
        rebuild(&pool).await.unwrap();
        sqlx::query("UPDATE folders SET is_pack = 1 WHERE path = '3D assets/Platformer Kit'")
            .execute(&pool)
            .await
            .unwrap();
        insert_path(&pool, "3D assets/Platformer Kit/Preview.png").await;
        rebuild(&pool).await.unwrap();

        let (is_pack, count): (bool, i64) = sqlx::query_as(
            "SELECT is_pack, item_count FROM folders WHERE path = '3D assets/Platformer Kit'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((is_pack, count), (true, 4));
    }

    #[tokio::test]
    async fn a_commands_pass_waits_for_the_index_run_holding_the_lock() {
        let pool = memory_library_pool().await;
        let library = crate::db::memory_library("lib", pool.clone());
        let db = std::sync::Arc::new(crate::db::memory_database(vec![library.clone()]).await);
        let lock = db.index_lock("lib");
        let run = lock.lock().await;

        let pass = tokio::spawn({
            let db = db.clone();
            async move { rebuild_library(&db, &library).await }
        });
        insert_path(&pool, "kits/kit/model.glb").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !pass.is_finished(),
            "the pass ran while an index run held the lock"
        );
        assert!(folder_rows(&pool).await.is_empty());

        drop(run);
        pass.await.unwrap();
        assert_eq!(
            folder_rows(&pool).await,
            fresh_pass(&["kits/kit/model.glb"]).await
        );
    }

    #[tokio::test]
    async fn a_pass_of_more_than_one_batch_writes_every_row() {
        let pool = memory_library_pool().await;
        let paths: Vec<String> = (0..450)
            .map(|i| format!("kits/kit {i}/model.glb"))
            .collect();
        for path in &paths {
            insert_path(&pool, path).await;
        }
        // The root, `kits` and 450 kits
        assert_eq!(rebuild(&pool).await.unwrap(), 452);
        sqlx::query("DELETE FROM assets WHERE file_path LIKE 'kits/kit 1%'")
            .execute(&pool)
            .await
            .unwrap();
        let rest: Vec<&str> = paths
            .iter()
            .map(String::as_str)
            .filter(|p| !p.starts_with("kits/kit 1"))
            .collect();
        // 111 kits go (1, 10-19, 100-199), and the root and `kits` change their count.
        assert_eq!(rebuild(&pool).await.unwrap(), 113);
        assert_eq!(folder_rows(&pool).await, fresh_pass(&rest).await);
    }
}
