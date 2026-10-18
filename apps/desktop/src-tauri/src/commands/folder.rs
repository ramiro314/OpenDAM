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
    pub is_pack: bool,
    /// Visible files below the folder, at any depth
    pub item_count: i64,
    /// Support files below the folder, at any depth, hidden unless "Show hidden files" is on
    pub hidden_count: i64,
    pub has_children: bool,
    /// Whether a pack lies below the folder
    pub holds_pack: bool,
    /// Whether a child has visible files or is or holds a pack, so it shows with hidden files off
    pub has_visible_children: bool,
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
/// their spaces, as on disk. A pack below a folder lies in the primary key range
/// `path/ <= p < path0` (`0` follows `/`), and makes the child holding it visible too.
pub(crate) async fn child_folders(
    pool: &Pool<Sqlite>,
    parent_path: &str,
) -> sqlx::Result<Vec<FolderNode>> {
    sqlx::query_as(
        "SELECT name, path, is_pack, item_count, hidden_count, has_children, holds_pack,
                holds_pack OR EXISTS (
                    SELECT 1 FROM folders c WHERE c.parent_path = n.path AND c.item_count > 0
                ) AS has_visible_children
         FROM (
             SELECT f.name, f.path, f.is_pack, f.item_count, f.hidden_count,
                    EXISTS (SELECT 1 FROM folders c WHERE c.parent_path = f.path) AS has_children,
                    EXISTS (
                        SELECT 1 FROM folders p
                        WHERE p.path >= f.path || '/' AND p.path < f.path || '0' AND p.is_pack
                    ) AS holds_pack
             FROM folders f
             WHERE f.parent_path = ?
         ) n
         ORDER BY name COLLATE NOCASE, path",
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

    /// A node with no pack in or below it and no hidden file.
    fn node(name: &str, path: &str, item_count: i64, has_children: bool) -> FolderNode {
        FolderNode {
            name: name.into(),
            path: path.into(),
            is_pack: false,
            item_count,
            hidden_count: 0,
            has_children,
            holds_pack: false,
            has_visible_children: has_children,
        }
    }

    #[tokio::test]
    async fn nodes_carry_packs_hidden_counts_and_whether_they_show_with_hidden_files_off() {
        let pool = library(&[
            "3D assets/Car Kit/License.txt",
            "3D assets/Car Kit/Models/sedan.png",
            "Audio/Casino Audio/License.txt",
            "Audio/Casino Audio/Preview.png",
            "Docs/Readme.txt",
            "Docs/Old/License.txt",
        ])
        .await;
        let top = child_folders(&pool, "").await.unwrap();
        let summary: Vec<(&str, bool, i64, i64, bool, bool)> = top
            .iter()
            .map(|n| {
                (
                    n.name.as_str(),
                    n.is_pack,
                    n.item_count,
                    n.hidden_count,
                    n.holds_pack,
                    n.has_visible_children,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("3D assets", false, 1, 1, true, true),
                // Every file of the pack below is hidden, but the pack still shows.
                ("Audio", false, 0, 2, true, true),
                // `Docs/Old` is a pack too, by its license.
                ("Docs", false, 1, 1, true, true),
            ]
        );
        let kit = child_folders(&pool, "3D assets").await.unwrap();
        assert_eq!(
            (
                kit[0].is_pack,
                kit[0].has_children,
                kit[0].has_visible_children
            ),
            (true, true, true)
        );
        let casino = child_folders(&pool, "Audio").await.unwrap();
        assert_eq!(
            (
                casino[0].is_pack,
                casino[0].item_count,
                casino[0].hidden_count
            ),
            (true, 0, 2)
        );
    }

    #[tokio::test]
    async fn a_folder_holding_only_support_files_has_no_visible_children() {
        let pool = library(&[
            "kit/License.txt",
            "kit/rock.png",
            "kit/Sources/Preview.png",
            "kit/Sources/game.txt",
        ])
        .await;
        let kit = &child_folders(&pool, "").await.unwrap()[0];
        assert_eq!(
            (
                kit.is_pack,
                kit.item_count,
                kit.hidden_count,
                kit.has_children
            ),
            (true, 1, 3, true)
        );
        assert!(!kit.has_visible_children);
        assert!(!kit.holds_pack);
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
