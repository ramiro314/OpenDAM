use serde::Serialize;
use sqlx::{Pool, Sqlite};
use std::sync::Arc;
use tauri::State;

use crate::AppState;

type Result<T> = std::result::Result<T, String>;

/// One node of a library's folder tree, as the structure pass derived it.
#[derive(Debug, PartialEq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct FolderNode {
    pub name: String,
    /// Relative to the library root
    pub path: String,
    /// Files below the folder, at any depth
    pub item_count: i64,
    pub has_children: bool,
}

/// The child folders of `parent_path` (`""` for the root) in library `library_id`, sorted by
/// name ignoring case. The sidebar tree loads each node's children with this as it expands.
#[tauri::command]
pub async fn get_folders(
    library_id: String,
    parent_path: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<FolderNode>> {
    let library = state
        .db
        .get_library_handle(&library_id)
        .await
        .ok_or_else(|| format!("Library {library_id} is not open"))?;
    child_folders(&library.pool, &parent_path)
        .await
        .map_err(|e| e.to_string())
}

/// Reads one node's children from `idx_folders_parent_name`; `path` breaks ties between names
/// that differ only in case. Only edge slashes are dropped from `parent_path`: folder names keep
/// their spaces, as on disk.
pub(crate) async fn child_folders(
    pool: &Pool<Sqlite>,
    parent_path: &str,
) -> sqlx::Result<Vec<FolderNode>> {
    sqlx::query_as(
        "SELECT f.name, f.path, f.item_count,
                EXISTS (SELECT 1 FROM folders c WHERE c.parent_path = f.path) AS has_children
         FROM folders f
         WHERE f.parent_path = ?
         ORDER BY f.name COLLATE NOCASE, f.path",
    )
    .bind(parent_path.trim_matches('/'))
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memory_library_pool;

    async fn library(paths: &[&str]) -> Pool<Sqlite> {
        let pool = memory_library_pool().await;
        for path in paths {
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
                 VALUES (?, ?, 'f', 'png', 'texture', 1, 0, 0)",
            )
            .bind(path)
            .bind(path)
            .execute(&pool)
            .await
            .unwrap();
        }
        crate::structure::rebuild(&pool).await.unwrap();
        pool
    }

    fn node(name: &str, path: &str, item_count: i64, has_children: bool) -> FolderNode {
        FolderNode {
            name: name.into(),
            path: path.into(),
            item_count,
            has_children,
        }
    }

    #[tokio::test]
    async fn the_root_lists_its_top_level_folders_by_name_ignoring_case() {
        let pool = library(&[
            "textures/rock.png",
            "Models/kit/a.png",
            "Models/b.png",
            "audio/x.png",
            "top.png",
        ])
        .await;
        let expected = [
            node("audio", "audio", 1, false),
            node("Models", "Models", 2, true),
            node("textures", "textures", 1, false),
        ];
        for root in ["", "/"] {
            assert_eq!(child_folders(&pool, root).await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn a_node_lists_only_its_own_children() {
        let pool = library(&[
            "3D assets/Platformer Kit/Models/a.png",
            "3D assets/Platformer Kit/b.png",
            "3D assets/Platformer Kit 2/c.png",
        ])
        .await;
        assert_eq!(
            child_folders(&pool, "3D assets/Platformer Kit/")
                .await
                .unwrap(),
            [node("Models", "3D assets/Platformer Kit/Models", 1, false)]
        );
        assert_eq!(child_folders(&pool, "nowhere").await.unwrap(), []);
    }

    #[tokio::test]
    async fn a_folder_named_with_edge_spaces_lists_its_own_children() {
        let pool = library(&[
            "Textures /Rock/a.png",
            "Textures/Wood/b.png",
            " Models/c.png",
        ])
        .await;
        assert_eq!(
            child_folders(&pool, "Textures ").await.unwrap(),
            [node("Rock", "Textures /Rock", 1, false)]
        );
        assert_eq!(
            child_folders(&pool, "Textures").await.unwrap(),
            [node("Wood", "Textures/Wood", 1, false)]
        );
        assert_eq!(child_folders(&pool, " Models").await.unwrap(), []);
        let top: Vec<String> = child_folders(&pool, "")
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.path)
            .collect();
        assert_eq!(top, [" Models", "Textures", "Textures "]);
    }

    #[tokio::test]
    async fn names_differing_only_in_case_keep_a_stable_order() {
        let pool = library(&["b/x.png", "B/y.png", "a/z.png"]).await;
        let names: Vec<String> = child_folders(&pool, "")
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.path)
            .collect();
        assert_eq!(names, ["a", "B", "b"]);
    }
}
