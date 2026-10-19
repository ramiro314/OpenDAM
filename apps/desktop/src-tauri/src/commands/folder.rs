use serde::Serialize;
use sqlx::{Pool, Sqlite};
use std::path::Path;
use std::sync::Arc;
use tauri::State;

use crate::db::{resolve_path, LibraryHandle};
use crate::structure::{LicenseKind, PackHeader};
use crate::AppState;

type Result<T> = std::result::Result<T, String>;

/// A pack's cover image, with what a card needs to show it as a grid tile shows an asset.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cover {
    /// Absolute
    pub file_path: String,
    /// Absolute, when the image has a thumbnail
    pub thumbnail_path: Option<String>,
    pub extension: String,
    pub kind: String,
}

/// The columns a cover is read from: its `folders` row `f` joined to the cover's asset row `c`
/// (`COVER_JOIN`), on the unique `file_path` index.
const COVER_COLUMNS: &str = "c.file_path AS cover_path, c.thumbnail_path AS cover_thumbnail,
    c.extension AS cover_extension, c.kind AS cover_kind";
const COVER_JOIN: &str = "LEFT JOIN assets c ON c.file_path = json_extract(f.pack_json, '$.cover')";

/// A cover's columns, as `COVER_COLUMNS` names them.
#[derive(sqlx::FromRow)]
struct CoverRow {
    cover_path: Option<String>,
    cover_thumbnail: Option<String>,
    cover_extension: Option<String>,
    cover_kind: Option<String>,
}

impl CoverRow {
    /// The cover with its paths made absolute below `root`, if the pack has one.
    fn into_cover(self, root: &Path) -> Option<Cover> {
        Some(Cover {
            file_path: resolve_path(root, &self.cover_path?),
            thumbnail_path: self.cover_thumbnail.map(|t| resolve_path(root, &t)),
            extension: self.cover_extension.unwrap_or_default(),
            kind: self.cover_kind.unwrap_or_default(),
        })
    }
}

/// One node of a library's folder tree, as the structure pass derived it.
#[derive(Debug, PartialEq, Serialize)]
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
    /// Whether the folder shows in the tree with hidden files off (`shown_without_hidden`)
    pub shown_without_hidden: bool,
    /// Whether a child shows with hidden files off, by the same rule
    pub has_visible_children: bool,
    /// A pack's cover image
    pub cover: Option<Cover>,
}

#[derive(sqlx::FromRow)]
struct NodeRow {
    name: String,
    path: String,
    is_pack: bool,
    item_count: i64,
    hidden_count: i64,
    has_children: bool,
    shown_without_hidden: bool,
    has_visible_children: bool,
    #[sqlx(flatten)]
    cover: CoverRow,
}

/// The one rule for whether folder `f` shows in the tree with hidden files off: it has visible
/// files, is a pack, or holds a pack, so every pack shows even when all its files are hidden. A
/// pack below `f` lies in the primary key range `f.path/ <= p < f.path0` (`0` follows `/`).
fn shown_without_hidden(f: &str) -> String {
    format!(
        "({f}.item_count > 0 OR {f}.is_pack OR EXISTS (
            SELECT 1 FROM folders p
            WHERE p.path >= {f}.path || '/' AND p.path < {f}.path || '0' AND p.is_pack
        ))"
    )
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
    child_folders(&library.pool, &library.root_path, &parent_path)
        .await
        .map_err(|e| e.to_string())
}

/// Reads one node's children from `idx_folders_parent_name`; `path` breaks ties between names
/// that differ only in case. Only edge slashes are dropped from `parent_path`: folder names keep
/// their spaces, as on disk. A node and its children show with hidden files off by the same
/// `shown_without_hidden` rule. Cover paths are made absolute below `root`.
pub(crate) async fn child_folders(
    pool: &Pool<Sqlite>,
    root: &Path,
    parent_path: &str,
) -> sqlx::Result<Vec<FolderNode>> {
    let sql = format!(
        "SELECT f.name, f.path, f.is_pack, f.item_count, f.hidden_count,
                EXISTS (SELECT 1 FROM folders k WHERE k.parent_path = f.path) AS has_children,
                {} AS shown_without_hidden,
                EXISTS (
                    SELECT 1 FROM folders k WHERE k.parent_path = f.path AND {}
                ) AS has_visible_children,
                {COVER_COLUMNS}
         FROM folders f {COVER_JOIN}
         WHERE f.parent_path = ?
         ORDER BY f.name COLLATE NOCASE, f.path",
        shown_without_hidden("f"),
        shown_without_hidden("k"),
    );
    let rows: Vec<NodeRow> = sqlx::query_as(&sql)
        .bind(parent_path.trim_matches('/'))
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| FolderNode {
            name: r.name,
            path: r.path,
            is_pack: r.is_pack,
            item_count: r.item_count,
            hidden_count: r.hidden_count,
            has_children: r.has_children,
            shown_without_hidden: r.shown_without_hidden,
            has_visible_children: r.has_visible_children,
            cover: r.cover.into_cover(root),
        })
        .collect())
}

/// One card of the Packs view.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackCard {
    pub library_id: String,
    /// Relative to the library root
    pub path: String,
    /// The name the structure pass gave the pack: its license header's, else its folder's
    pub name: String,
    /// The folder holding the pack, the card's subtitle; `''` for the library root
    pub parent_path: String,
    /// Visible files below the pack; 0 shows as "No visible items"
    pub item_count: i64,
    pub hidden_count: i64,
    pub cover: Option<Cover>,
}

#[derive(sqlx::FromRow)]
struct PackRow {
    path: String,
    parent_path: Option<String>,
    folder_name: String,
    pack_name: Option<String>,
    item_count: i64,
    hidden_count: i64,
    #[sqlx(flatten)]
    cover: CoverRow,
}

/// Every pack below `under_path` (`""` for the root) in each library of `library_ids`, or in
/// every open library when it is `None`, in one list sorted by name ignoring case (D20). Below
/// "All libraries" (`None` and `""`) lies each library's root, so a library that is itself one
/// pack has a card there.
#[tauri::command]
pub async fn get_packs(
    library_ids: Option<Vec<String>>,
    under_path: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<PackCard>> {
    let library_ids_none = library_ids.is_none();
    let libraries = match library_ids {
        None => state.db.get_all_libraries().await,
        Some(ids) => {
            let mut open = Vec::new();
            for id in ids {
                open.push(
                    state
                        .db
                        .get_library_handle(&id)
                        .await
                        .ok_or_else(|| format!("Library {id} is not open"))?,
                );
            }
            open
        }
    };
    let all_libraries = library_ids_none && under_path.trim_matches('/').is_empty();
    packs_below(&libraries, &under_path, all_libraries)
        .await
        .map_err(|e| e.to_string())
}

/// The packs strictly below `under_path` in each of `libraries`, plus each library's root when
/// `with_roots` (below "All libraries"), sorted by name, folding ASCII case only as `NOCASE`
/// does, then by path, then by the library's place in `libraries`. A root pack the pass gave no
/// name (no license header) takes its library's.
pub(crate) async fn packs_below(
    libraries: &[LibraryHandle],
    under_path: &str,
    with_roots: bool,
) -> sqlx::Result<Vec<PackCard>> {
    let under = under_path.trim_matches('/');
    // Below the root is every folder but the root; below `p`, the key range `p/ <= path < p0`.
    let below = if under.is_empty() {
        if with_roots {
            "1"
        } else {
            "f.path <> ''"
        }
    } else {
        "f.path >= ?1 || '/' AND f.path < ?1 || '0'"
    };
    let sql = format!(
        "SELECT f.path, f.parent_path, f.name AS folder_name,
                json_extract(f.pack_json, '$.name') AS pack_name,
                f.item_count, f.hidden_count, {COVER_COLUMNS}
         FROM folders f {COVER_JOIN}
         WHERE f.is_pack AND {below}"
    );
    let mut cards = Vec::new();
    for library in libraries {
        let mut query = sqlx::query_as::<_, PackRow>(&sql);
        if !under.is_empty() {
            query = query.bind(under);
        }
        let rows = query.fetch_all(&library.pool).await?;
        cards.extend(rows.into_iter().map(|r| {
            PackCard {
                library_id: library.id.clone(),
                path: r.path,
                name: [r.pack_name.unwrap_or_default(), r.folder_name]
                    .into_iter()
                    .find(|n| !n.is_empty())
                    .unwrap_or_else(|| library.name.clone()),
                parent_path: r.parent_path.unwrap_or_default(),
                item_count: r.item_count,
                hidden_count: r.hidden_count,
                cover: r.cover.into_cover(&library.root_path),
            }
        }));
    }
    // Stable, so packs that tie on name and path keep their libraries' order.
    cards.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(cards)
}

/// What the detail panel shows for one pack (plan C18), as the structure pass derived it (C9).
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackDetail {
    pub library_id: String,
    /// Relative to the library root
    pub path: String,
    /// The license header's name, else the folder's, else the library's for a root pack
    pub name: String,
    /// The folder holding the pack; `''` for the library root
    pub parent_path: String,
    pub item_count: i64,
    pub hidden_count: i64,
    pub cover: Option<Cover>,
    /// The pack's `Preview*` and `Sample*` images, by path
    pub gallery: Vec<Cover>,
    pub license: Option<PackLicense>,
    /// The Kenney header of the license file, if it has one
    pub header: Option<PackHeader>,
    /// Files below the pack by kind, support files and group members included
    pub kinds: std::collections::BTreeMap<String, i64>,
    /// Their total size in bytes
    pub size_bytes: i64,
}

/// A pack's license file, as "Open license" opens it.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackLicense {
    /// Absolute
    pub file_path: String,
    pub kind: LicenseKind,
}

/// The pack that holds `path` in library `library_id`: the innermost pack at or above it, the
/// folder itself included, or `None` when no pack holds it. A pack card asks with the pack's own
/// path; the detail panel asks with the selected folder, `''` for a library's root.
#[tauri::command]
pub async fn get_pack_detail(
    library_id: String,
    path: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Option<PackDetail>> {
    let library = state
        .db
        .get_library_handle(&library_id)
        .await
        .ok_or_else(|| format!("Library {library_id} is not open"))?;
    pack_holding(&library, &path)
        .await
        .map_err(|e| e.to_string())
}

#[derive(sqlx::FromRow)]
struct PackDetailRow {
    path: String,
    parent_path: Option<String>,
    name: String,
    item_count: i64,
    hidden_count: i64,
    pack_json: Option<String>,
}

/// `get_pack_detail` on an open library. The pack is found among `path` and its ancestors on the
/// primary key, and its cover and gallery images on the unique `file_path` index.
pub(crate) async fn pack_holding(
    library: &LibraryHandle,
    path: &str,
) -> sqlx::Result<Option<PackDetail>> {
    let path = path.trim_matches('/');
    let mut candidates = vec![String::new()];
    candidates.extend(
        path.match_indices('/')
            .map(|(end, _)| path[..end].to_string()),
    );
    if !path.is_empty() {
        candidates.push(path.to_string());
    }
    let row: Option<PackDetailRow> = sqlx::query_as(
        "SELECT path, parent_path, name, item_count, hidden_count, pack_json FROM folders
         WHERE is_pack AND path IN (SELECT value FROM json_each(?))
         ORDER BY length(path) DESC LIMIT 1",
    )
    .bind(serde_json::to_string(&candidates).expect("paths serialize"))
    .fetch_optional(&library.pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let info = crate::structure::parse_pack(row.pack_json.as_deref()).unwrap_or_default();

    let images: Vec<&str> = info
        .cover
        .iter()
        .chain(&info.gallery)
        .map(String::as_str)
        .collect();
    let covers: Vec<(String, Option<String>, String, String)> = sqlx::query_as(
        "SELECT file_path, thumbnail_path, extension, kind FROM assets
         WHERE file_path IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::to_string(&images).expect("paths serialize"))
    .fetch_all(&library.pool)
    .await?;
    let root = &library.root_path;
    let cover_of = |path: &str| {
        covers
            .iter()
            .find(|(p, ..)| p == path)
            .map(|(p, thumbnail, extension, kind)| Cover {
                file_path: resolve_path(root, p),
                thumbnail_path: thumbnail.as_deref().map(|t| resolve_path(root, t)),
                extension: extension.clone(),
                kind: kind.clone(),
            })
    };

    Ok(Some(PackDetail {
        library_id: library.id.clone(),
        name: [info.name, row.name]
            .into_iter()
            .find(|n| !n.is_empty())
            .unwrap_or_else(|| library.name.clone()),
        path: row.path,
        parent_path: row.parent_path.unwrap_or_default(),
        item_count: row.item_count,
        hidden_count: row.hidden_count,
        cover: info.cover.as_deref().and_then(cover_of),
        gallery: info.gallery.iter().filter_map(|p| cover_of(p)).collect(),
        header: info.license.as_ref().and_then(|l| l.text.header.clone()),
        license: info.license.map(|l| PackLicense {
            file_path: resolve_path(root, &l.path),
            kind: l.text.kind,
        }),
        kinds: info.kinds,
        size_bytes: info.size_bytes,
    }))
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
        crate::structure::rebuild(&pool, crate::structure::tests::unmounted())
            .await
            .unwrap();
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
            shown_without_hidden: item_count > 0,
            has_visible_children: has_children,
            cover: None,
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
        let top = child_folders(&pool, Path::new("/lib"), "").await.unwrap();
        let summary: Vec<(&str, bool, i64, i64, bool, bool)> = top
            .iter()
            .map(|n| {
                (
                    n.name.as_str(),
                    n.is_pack,
                    n.item_count,
                    n.hidden_count,
                    n.shown_without_hidden,
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
        let kit = child_folders(&pool, Path::new("/lib"), "3D assets")
            .await
            .unwrap();
        assert_eq!(
            (
                kit[0].is_pack,
                kit[0].has_children,
                kit[0].has_visible_children
            ),
            (true, true, true)
        );
        let casino = child_folders(&pool, Path::new("/lib"), "Audio")
            .await
            .unwrap();
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
    async fn a_node_has_visible_children_exactly_when_a_child_shows_without_hidden_files() {
        let pool = library(&[
            "3D assets/Car Kit/License.txt",
            "3D assets/Car Kit/Models/sedan.png",
            "3D assets/Car Kit/Sources/Preview.png",
            "3D assets/Car Kit/Sources/notes.txt",
            "Audio/Casino Audio/License.txt",
            "Archive/Readme.txt",
            "Archive/Old/Kit/License.txt",
            "Archive/Old/Kit/a.png",
            "Goodies/Wallpapers/w.png",
        ])
        .await;
        let mut queue = vec![String::new()];
        let mut nodes = 0;
        while let Some(parent) = queue.pop() {
            for node in child_folders(&pool, Path::new("/lib"), &parent)
                .await
                .unwrap()
            {
                let children = child_folders(&pool, Path::new("/lib"), &node.path)
                    .await
                    .unwrap();
                assert_eq!(
                    node.has_visible_children,
                    children.iter().any(|c| c.shown_without_hidden),
                    "{}",
                    node.path
                );
                nodes += 1;
                queue.push(node.path);
            }
        }
        assert_eq!(nodes, 11);
        // `Sources` holds only hidden files and is no pack, so it alone is left out.
        let sources = child_folders(&pool, Path::new("/lib"), "3D assets/Car Kit")
            .await
            .unwrap();
        let shown: Vec<(&str, bool)> = sources
            .iter()
            .map(|n| (n.name.as_str(), n.shown_without_hidden))
            .collect();
        assert_eq!(shown, [("Models", true), ("Sources", false)]);
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
        let kit = &child_folders(&pool, Path::new("/lib"), "").await.unwrap()[0];
        assert_eq!(
            (
                kit.is_pack,
                kit.item_count,
                kit.hidden_count,
                kit.has_children
            ),
            (true, 1, 3, true)
        );
        assert!(kit.shown_without_hidden);
        assert!(!kit.has_visible_children);
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
            assert_eq!(
                child_folders(&pool, Path::new("/lib"), root).await.unwrap(),
                expected
            );
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
            child_folders(&pool, Path::new("/lib"), "3D assets/Platformer Kit/")
                .await
                .unwrap(),
            [node("Models", "3D assets/Platformer Kit/Models", 1, false)]
        );
        assert_eq!(
            child_folders(&pool, Path::new("/lib"), "nowhere")
                .await
                .unwrap(),
            []
        );
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
            child_folders(&pool, Path::new("/lib"), "Textures ")
                .await
                .unwrap(),
            [node("Rock", "Textures /Rock", 1, false)]
        );
        assert_eq!(
            child_folders(&pool, Path::new("/lib"), "Textures")
                .await
                .unwrap(),
            [node("Wood", "Textures/Wood", 1, false)]
        );
        assert_eq!(
            child_folders(&pool, Path::new("/lib"), " Models")
                .await
                .unwrap(),
            []
        );
        let top: Vec<String> = child_folders(&pool, Path::new("/lib"), "")
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
        let names: Vec<String> = child_folders(&pool, Path::new("/lib"), "")
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.path)
            .collect();
        assert_eq!(names, ["a", "B", "b"]);
    }

    /// A library `id` at `/libraries/<id>` holding `paths`, with its pass run.
    async fn library_handle(id: &str, paths: &[&str]) -> LibraryHandle {
        let mut library = crate::db::memory_library(id, library(paths).await);
        library.root_path = format!("/libraries/{id}").into();
        library
    }

    /// Each card as `(library, path, name, parent, items)`.
    fn summary(cards: &[PackCard]) -> Vec<(&str, &str, &str, &str, i64)> {
        cards
            .iter()
            .map(|c| {
                (
                    c.library_id.as_str(),
                    c.path.as_str(),
                    c.name.as_str(),
                    c.parent_path.as_str(),
                    c.item_count,
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn the_packs_below_a_node_come_flat_and_by_name_across_libraries() {
        let a = library_handle(
            "a",
            &[
                "3D assets/space kit/License.txt",
                "3D assets/space kit/rocket.png",
                "3D assets/Car Kit/License.txt",
                "3D assets/Car Kit/Preview.png",
                "3D assets/Car Kit/Models/sedan.png",
                "Audio/Casino Audio/License.txt",
                "Audio/Casino Audio/Preview.png",
                "Goodies/w.png",
            ],
        )
        .await;
        let b = library_handle(
            "b",
            &["Kits/Car Kit/License.txt", "Kits/Boat Kit/Sample.png"],
        )
        .await;
        let libraries = [a, b];

        let all = packs_below(&libraries, "", false).await.unwrap();
        assert_eq!(
            summary(&all),
            [
                ("b", "Kits/Boat Kit", "Boat Kit", "Kits", 0),
                ("a", "3D assets/Car Kit", "Car Kit", "3D assets", 1),
                ("b", "Kits/Car Kit", "Car Kit", "Kits", 0),
                // Every file hidden: the card shows "No visible items".
                ("a", "Audio/Casino Audio", "Casino Audio", "Audio", 0),
                ("a", "3D assets/space kit", "space kit", "3D assets", 1),
            ]
        );
        let car = &all[1];
        assert_eq!(
            car.cover,
            Some(Cover {
                file_path: "/libraries/a/3D assets/Car Kit/Preview.png".into(),
                thumbnail_path: None,
                extension: "png".into(),
                kind: "texture".into(),
            })
        );
        // The first image stands in when the pack has no preview or sample.
        assert_eq!(
            all[4].cover.as_ref().map(|c| c.file_path.as_str()),
            Some("/libraries/a/3D assets/space kit/rocket.png")
        );
        // A pack whose only file is its license has no cover.
        assert_eq!(all[2].cover, None);

        let below = packs_below(&libraries, "3D assets/", false).await.unwrap();
        assert_eq!(
            below.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
            ["3D assets/Car Kit", "3D assets/space kit"]
        );
        // A pack itself has no packs below it, nor does a folder holding none.
        assert_eq!(
            packs_below(&libraries, "3D assets/Car Kit", false)
                .await
                .unwrap(),
            []
        );
        assert_eq!(packs_below(&libraries, "Goodies", false).await.unwrap(), []);
        assert_eq!(
            packs_below(&libraries[1..], "3D assets", false)
                .await
                .unwrap(),
            []
        );
    }

    #[tokio::test]
    async fn a_library_that_is_one_pack_has_a_card_only_below_all_libraries() {
        let mut solo = library_handle("solo", &["License.txt", "Preview.png", "rock.png"]).await;
        solo.name = "Solo Download".into();
        let kits = library_handle("kits", &["3D assets/Car Kit/License.txt"]).await;
        let libraries = [kits, solo];

        // Below "All libraries", the root pack, named after its library
        let all = packs_below(&libraries, "", true).await.unwrap();
        assert_eq!(
            summary(&all),
            [
                ("kits", "3D assets/Car Kit", "Car Kit", "3D assets", 0),
                ("solo", "", "Solo Download", "", 1),
            ]
        );
        assert_eq!(
            all[1].cover.as_ref().map(|c| c.file_path.as_str()),
            Some("/libraries/solo/Preview.png")
        );
        // The library row itself is that pack, so it has no pack below it.
        assert_eq!(packs_below(&libraries[1..], "", false).await.unwrap(), []);

        // A license header's name wins over the library's.
        sqlx::query("UPDATE folders SET pack_json = json_set(pack_json, '$.name', 'Rock Kit') WHERE path = ''")
            .execute(&libraries[1].pool)
            .await
            .unwrap();
        let named = packs_below(&libraries, "", true).await.unwrap();
        assert_eq!(named[1].name, "Rock Kit");
    }

    #[tokio::test]
    async fn a_card_shows_the_pass_name_and_the_covers_thumbnail() {
        let library = library_handle("a", &["kits/kit/License.txt", "kits/kit/Preview.tga"]).await;
        sqlx::query(
            "UPDATE folders SET pack_json = json_set(pack_json, '$.name', 'Kit Deluxe')
             WHERE path = 'kits/kit'",
        )
        .execute(&library.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE assets SET thumbnail_path = '.opendam/thumbs/p_thumb.png', extension = 'tga'
             WHERE file_path = 'kits/kit/Preview.tga'",
        )
        .execute(&library.pool)
        .await
        .unwrap();
        let cards = packs_below(std::slice::from_ref(&library), "", false)
            .await
            .unwrap();
        assert_eq!(cards[0].name, "Kit Deluxe");
        let cover = cards[0].cover.as_ref().unwrap();
        assert_eq!(
            (cover.thumbnail_path.as_deref(), cover.extension.as_str()),
            (Some("/libraries/a/.opendam/thumbs/p_thumb.png"), "tga")
        );
        // The tree's pack node carries the same cover, and keeps its folder's name.
        let node = &child_folders(&library.pool, &library.root_path, "kits")
            .await
            .unwrap()[0];
        assert_eq!(
            (node.name.as_str(), node.cover.as_ref()),
            ("kit", Some(cover))
        );
    }

    #[tokio::test]
    async fn a_packs_detail_is_found_from_the_pack_or_any_folder_inside_it() {
        let library = library_handle(
            "a",
            &[
                "3D assets/Car Kit/License.txt",
                "3D assets/Car Kit/Preview.png",
                "3D assets/Car Kit/Sample.png",
                "3D assets/Car Kit/Models/sedan.png",
                "Goodies/w.png",
            ],
        )
        .await;
        sqlx::query(
            r#"UPDATE folders SET pack_json = json_set(pack_json,
                 '$.name', 'Car Kit',
                 '$.license.kind', 'CC0',
                 '$.license.header', json('{"name":"Car Kit","version":"3.1","author":"Kenney"}'))
             WHERE path = '3D assets/Car Kit'"#,
        )
        .execute(&library.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE assets SET thumbnail_path = '.opendam/thumbs/s_thumb.png'
             WHERE file_path = '3D assets/Car Kit/Sample.png'",
        )
        .execute(&library.pool)
        .await
        .unwrap();

        let image = |path: &str, thumbnail: Option<&str>| Cover {
            file_path: format!("/libraries/a/{path}"),
            thumbnail_path: thumbnail.map(|t| format!("/libraries/a/{t}")),
            extension: "png".into(),
            kind: "texture".into(),
        };
        let detail = pack_holding(&library, "3D assets/Car Kit").await.unwrap();
        assert_eq!(
            detail,
            Some(PackDetail {
                library_id: "a".into(),
                path: "3D assets/Car Kit".into(),
                name: "Car Kit".into(),
                parent_path: "3D assets".into(),
                item_count: 1,
                hidden_count: 3,
                cover: Some(image("3D assets/Car Kit/Preview.png", None)),
                gallery: vec![
                    image("3D assets/Car Kit/Preview.png", None),
                    image(
                        "3D assets/Car Kit/Sample.png",
                        Some(".opendam/thumbs/s_thumb.png")
                    ),
                ],
                license: Some(PackLicense {
                    file_path: "/libraries/a/3D assets/Car Kit/License.txt".into(),
                    kind: LicenseKind::Cc0,
                }),
                header: Some(PackHeader {
                    name: "Car Kit".into(),
                    version: Some("3.1".into()),
                    author: Some("Kenney".into()),
                }),
                // Kinds by extension, as the indexer gives them; the test library's files are
                // 1 byte each.
                kinds: [("document".to_string(), 1), ("texture".to_string(), 3)].into(),
                size_bytes: 4,
            })
        );
        // From a folder inside the pack, or with edge slashes
        assert_eq!(
            pack_holding(&library, "/3D assets/Car Kit/Models/")
                .await
                .unwrap(),
            detail
        );
        // No pack holds a folder outside every pack, or the library root.
        for path in ["Goodies", "3D assets", "", "3D assets/Car"] {
            assert_eq!(pack_holding(&library, path).await.unwrap(), None, "{path}");
        }
    }

    #[tokio::test]
    async fn a_library_that_is_one_pack_shows_it_at_its_root_named_after_the_library() {
        let mut solo = library_handle("solo", &["License.txt", "rock.png", "Tiles/a.png"]).await;
        solo.name = "Solo Download".into();
        for path in ["", "Tiles"] {
            let detail = pack_holding(&solo, path).await.unwrap().unwrap();
            assert_eq!(
                (detail.path.as_str(), detail.name.as_str()),
                ("", "Solo Download")
            );
            // A license the pass couldn't read is unknown, with no header.
            assert_eq!(
                (detail.license.map(|l| l.kind), detail.header),
                (Some(LicenseKind::Unknown), None)
            );
        }
    }
}
