use serde::{Deserialize, Serialize};
use sqlx::{Pool, Sqlite};
use std::path::Path;
use std::sync::Arc;
use tauri::{Emitter, State};

use crate::commands::asset::folder_range;
use crate::db::{resolve_path, upsert_collection, upsert_tag, Database, LibraryHandle};
use crate::structure::{self, LicenseKind, PackHeader, PackMark, PackOverride};
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
/// Joins a `folders` row `f` to your override of it `o`, for its mark and name.
const MARK_JOIN: &str = "LEFT JOIN pack_overrides o ON o.path = f.path";

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
    /// What you said the folder is ("Mark as pack", "Not a pack"), if you did
    pub mark: Option<PackMark>,
    /// The name you gave the pack, which the tree shows in place of the folder's
    pub renamed: Option<String>,
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
    mark: Option<PackMark>,
    renamed: Option<String>,
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
                {COVER_COLUMNS}, o.mark, CASE WHEN f.is_pack THEN o.name END AS renamed
         FROM folders f {COVER_JOIN} {MARK_JOIN}
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
            mark: r.mark,
            renamed: r.renamed,
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
    /// The name the structure pass gave the pack: yours, else its license header's, else its
    /// folder's
    pub name: String,
    /// The folder holding the pack, the card's subtitle; `''` for the library root
    pub parent_path: String,
    /// Visible files below the pack; 0 shows as "No visible items"
    pub item_count: i64,
    pub hidden_count: i64,
    pub cover: Option<Cover>,
    /// What you said the folder is, if you did
    pub mark: Option<PackMark>,
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
    mark: Option<PackMark>,
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
                f.item_count, f.hidden_count, {COVER_COLUMNS}, o.mark
         FROM folders f {COVER_JOIN} {MARK_JOIN}
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
                mark: r.mark,
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
    /// Your name, else the license header's, else the folder's, else the library's for a root
    /// pack
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
    /// Your edits to the pack, all `None` when you made none. The edit form starts from them,
    /// and the panel shows your license and source URL.
    pub edits: PackOverride,
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
    let edits: PackOverride = sqlx::query_as(
        "SELECT mark, name, license, source_url, cover_path FROM pack_overrides WHERE path = ?",
    )
    .bind(&row.path)
    .fetch_optional(&library.pool)
    .await?
    .unwrap_or_default();

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
        edits,
    }))
}

/// One edit to a folder's pack (plan C18), stored in `pack_overrides`.
#[derive(Debug, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PackEdit {
    /// "Mark as pack" or "Not a pack", or `None` to leave the folder to the pack rules
    Mark { mark: Option<PackMark> },
    /// The edit form's fields. An empty one keeps what the structure pass derives.
    Details {
        name: Option<String>,
        license: Option<String>,
        source_url: Option<String>,
    },
    /// "Use as pack cover" with an image below the pack, or `None` for the cover the pass picks
    Cover { asset_id: Option<String> },
}

/// Saves `edit` to the folder at `path` (`""` for the root) in library `library_id`, reruns the
/// structure pass so the tree, the Packs view and the panel show it, then emits
/// `library:updated`.
#[tauri::command]
pub async fn set_pack_override(
    library_id: String,
    path: String,
    edit: PackEdit,
    state: State<'_, Arc<AppState>>,
) -> Result<()> {
    set_pack_override_in(&state.db, &library_id, &path, edit).await?;
    let _ = state.app_handle.emit("library:updated", ());
    Ok(())
}

/// A text field as stored: trimmed, and `None` when empty.
fn field(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `set_pack_override` without the event. The edit and the removal of a row it leaves empty
/// commit together, then the pass runs once any index run on the library has finished
/// (`structure::rebuild_library`). The pass never deletes a row.
pub(crate) async fn set_pack_override_in(
    db: &Database,
    library_id: &str,
    path: &str,
    edit: PackEdit,
) -> Result<()> {
    let lib = open_library(db, library_id).await?;
    let path = path.trim_matches('/');
    let pool = &lib.pool;
    let found: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM folders WHERE path = ?)")
        .bind(path)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?;
    if !found {
        return Err(format!("There is no folder {path} in this library"));
    }
    let now = chrono::Utc::now().timestamp_millis();
    let write = match edit {
        PackEdit::Mark { mark } => sqlx::query(
            "INSERT INTO pack_overrides (path, mark, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(path) DO UPDATE SET mark = excluded.mark, updated_at = excluded.updated_at",
        )
        .bind(path)
        .bind(mark),
        PackEdit::Details {
            name,
            license,
            source_url,
        } => {
            let source_url = field(source_url);
            if source_url
                .as_deref()
                .is_some_and(|url| !is_web_url(url))
            {
                return Err("The source URL must start with http:// or https://".into());
            }
            sqlx::query(
                "INSERT INTO pack_overrides (path, name, license, source_url, updated_at)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(path) DO UPDATE SET name = excluded.name,
                     license = excluded.license, source_url = excluded.source_url,
                     updated_at = excluded.updated_at",
            )
            .bind(path)
            .bind(field(name))
            .bind(field(license))
            .bind(source_url)
        }
        PackEdit::Cover { asset_id } => {
            let cover = match asset_id {
                None => None,
                Some(id) => Some(cover_below(pool, path, &id).await?),
            };
            sqlx::query(
                "INSERT INTO pack_overrides (path, cover_path, updated_at) VALUES (?, ?, ?)
                 ON CONFLICT(path) DO UPDATE SET cover_path = excluded.cover_path,
                     updated_at = excluded.updated_at",
            )
            .bind(path)
            .bind(cover)
        }
    };
    let saved = async {
        let mut tx = pool.begin().await?;
        write.bind(now).execute(&mut *tx).await?;
        sqlx::query(
            "DELETE FROM pack_overrides WHERE path = ? AND mark IS NULL AND name IS NULL
                 AND license IS NULL AND source_url IS NULL AND cover_path IS NULL",
        )
        .bind(path)
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    };
    saved.await.map_err(|e| e.to_string())?;
    structure::rebuild_library(db, &lib).await;
    Ok(())
}

/// Whether `url` is a web address, the only kind a source URL may be and "Open" opens.
pub(crate) fn is_web_url(url: &str) -> bool {
    let url = url.to_ascii_lowercase();
    ["http://", "https://"]
        .iter()
        .any(|scheme| url.len() > scheme.len() && url.starts_with(scheme))
}

/// The path of asset `id` when it is an image below the folder `pack` (`""` for the root), for
/// "Use as pack cover".
async fn cover_below(pool: &Pool<Sqlite>, pack: &str, id: &str) -> Result<String> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT file_path, kind FROM assets WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?;
    let Some((path, kind)) = row else {
        return Err("That file is not in this library".into());
    };
    if !matches!(kind.as_str(), "texture" | "image") {
        return Err("Only an image can be a pack's cover".into());
    }
    if !(pack.is_empty() || path.starts_with(&format!("{pack}/"))) {
        return Err("That image is not in this pack".into());
    }
    Ok(path)
}

/// A pack by its library and its path relative to the library root, with its name.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackRef {
    pub library_id: String,
    pub path: String,
    pub name: String,
}

/// The pack "Use as pack cover" on asset `asset_id` sets the cover of: the innermost pack
/// holding it, or `None` when the asset is no image or no pack holds it.
#[tauri::command]
pub async fn get_cover_pack(
    asset_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Option<PackRef>> {
    cover_pack(&state.db, &asset_id)
        .await
        .map_err(|e| e.to_string())
}

pub(crate) async fn cover_pack(db: &Database, asset_id: &str) -> sqlx::Result<Option<PackRef>> {
    let Some(library) = db.library_of_asset(asset_id).await? else {
        return Ok(None);
    };
    let (path, kind): (String, String) =
        sqlx::query_as("SELECT file_path, kind FROM assets WHERE id = ?")
            .bind(asset_id)
            .fetch_one(&library.pool)
            .await?;
    if !matches!(kind.as_str(), "texture" | "image") {
        return Ok(None);
    }
    let (dir, _) = path.rsplit_once('/').unwrap_or(("", &path));
    Ok(pack_holding(&library, dir).await?.map(|pack| PackRef {
        library_id: pack.library_id,
        path: pack.path,
        name: pack.name,
    }))
}

/// A file a bulk write reaches, and the grid item it belongs to: its group's main item, or itself
/// for a file on its own.
type FileOfItem = (String, String);

/// Every file below `folder` (`""` for the library root, which covers the whole library), with
/// every member of the groups those files belong to, wherever it sits, so a bulk write keeps a
/// group's members equal (plan C20). Support files are included: the write covers everything.
/// Sorted by item, so each group's members come together for `group_batches`.
async fn folder_members(pool: &Pool<Sqlite>, folder: &str) -> sqlx::Result<Vec<FileOfItem>> {
    let Some([from, to]) = folder_range(folder) else {
        return sqlx::query_as(
            "SELECT id, COALESCE(group_id, id) AS item FROM assets ORDER BY item, id",
        )
        .fetch_all(pool)
        .await;
    };
    sqlx::query_as(
        "SELECT id, COALESCE(group_id, id) AS item FROM assets
         WHERE file_path >= ?1 AND file_path < ?2
            OR group_id IN (
                SELECT group_id FROM assets
                WHERE file_path >= ?1 AND file_path < ?2 AND group_id IS NOT NULL
            )
         ORDER BY item, id",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
}

/// Cuts `files`, sorted by item, into batches of about `structure::BATCH` files that each end at
/// an item's last file, so a group's members always commit together.
fn group_batches(files: &[FileOfItem]) -> Vec<&[FileOfItem]> {
    let mut batches = Vec::new();
    let mut start = 0;
    for end in 1..=files.len() {
        let full = end - start >= structure::BATCH;
        if end == files.len() || (full && files[end].1 != files[end - 1].1) {
            batches.push(&files[start..end]);
            start = end;
        }
    }
    batches
}

/// The open library `library_id`.
async fn open_library(db: &Database, library_id: &str) -> Result<LibraryHandle> {
    db.get_library_handle(library_id)
        .await
        .ok_or_else(|| format!("Library {library_id} is not open"))
}

/// "Add a tag to everything": tags every file below `folder_path` in library `library_id`, and
/// the members of their groups (`folder_members`). Returns how many files that is.
#[tauri::command]
pub async fn tag_folder(
    library_id: String,
    folder_path: String,
    tag_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<usize> {
    tag_folder_in(&state.db, &library_id, &folder_path, &tag_id).await
}

/// Writes in transactions of about `structure::BATCH` files that never split a group
/// (`group_batches`), each copying master's definition, with its ancestors root first, into the
/// library's mirror before the memberships, since a mirror can lack it. It holds the library's
/// index lock, so no structure pass regroups the files meanwhile, and stops at the first batch
/// that finds the tag deleted. Each batch holds `Database::catalog_lock` from reading the
/// definition until its commit, so a tag update or delete waits for at most one batch.
async fn tag_folder_in(
    db: &Database,
    library_id: &str,
    folder: &str,
    tag_id: &str,
) -> Result<usize> {
    let lib = open_library(db, library_id).await?;
    let lock = db.index_lock(&lib.id);
    let _guard = lock.lock().await;
    // The definition is read once the lock is held, and again for each batch under
    // `Database::catalog_lock`, which every definition update and delete takes too: one deleted
    // while the write waited, or between batches, must not come back through the mirror.
    let write = async {
        if db.tag_with_ancestors(tag_id).await?.is_empty() {
            return Ok(None);
        }
        let files = folder_members(&lib.pool, folder).await?;
        for batch in group_batches(&files) {
            let _catalog = db.catalog_lock.lock().await;
            let chain = db.tag_with_ancestors(tag_id).await?;
            if chain.is_empty() {
                return Ok(None);
            }
            let mut tx = lib.pool.begin().await?;
            for definition in &chain {
                upsert_tag(&mut *tx, definition).await?;
            }
            for (id, _) in batch {
                sqlx::query("INSERT OR IGNORE INTO asset_tags (asset_id, tag_id) VALUES (?, ?)")
                    .bind(id)
                    .bind(tag_id)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
        }
        Ok::<_, sqlx::Error>(Some(files.len()))
    };
    match write.await {
        Ok(Some(written)) => Ok(written),
        Ok(None) => Err("The tag no longer exists".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// "Add everything to a collection": adds every file below `folder_path` in library
/// `library_id`, and the members of their groups (`folder_members`), to collection
/// `collection_id`. Returns how many files that is.
#[tauri::command]
pub async fn collect_folder(
    library_id: String,
    folder_path: String,
    collection_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<usize> {
    collect_folder_in(&state.db, &library_id, &folder_path, &collection_id).await
}

/// Writes as `tag_folder_in` does, copying the collection's definition and its ancestors.
async fn collect_folder_in(
    db: &Database,
    library_id: &str,
    folder: &str,
    collection_id: &str,
) -> Result<usize> {
    let lib = open_library(db, library_id).await?;
    let lock = db.index_lock(&lib.id);
    let _guard = lock.lock().await;
    // The definition is read once the lock is held, and again for each batch under
    // `Database::catalog_lock`, which every definition update and delete takes too: one deleted
    // while the write waited, or between batches, must not come back through the mirror.
    let write = async {
        if db
            .collection_with_ancestors(collection_id)
            .await?
            .is_empty()
        {
            return Ok(None);
        }
        let files = folder_members(&lib.pool, folder).await?;
        for batch in group_batches(&files) {
            let _catalog = db.catalog_lock.lock().await;
            let chain = db.collection_with_ancestors(collection_id).await?;
            if chain.is_empty() {
                return Ok(None);
            }
            let mut tx = lib.pool.begin().await?;
            for definition in &chain {
                upsert_collection(&mut *tx, definition).await?;
            }
            for (id, _) in batch {
                sqlx::query(
                    "INSERT OR IGNORE INTO collection_assets (collection_id, asset_id) VALUES (?, ?)",
                )
                .bind(collection_id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
        }
        Ok::<_, sqlx::Error>(Some(files.len()))
    };
    match write.await {
        Ok(Some(written)) => Ok(written),
        Ok(None) => Err("The collection no longer exists".into()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        insert_asset, insert_collection, insert_tag, memory_database, memory_library,
        memory_library_pool, memory_library_pool_enforcing_foreign_keys,
    };
    use crate::structure::tests::{SEDAN, SEDAN_RENDER};

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
            mark: None,
            renamed: None,
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
                edits: PackOverride::default(),
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
            // Every file counts once, those at the root included.
            assert_eq!(
                (detail.kinds, detail.size_bytes),
                (
                    [("document".to_string(), 1), ("texture".to_string(), 2)].into(),
                    3
                )
            );
        }
    }

    #[tokio::test]
    async fn the_pack_holding_a_folder_is_the_innermost_of_nested_packs() {
        let library = library_handle(
            "a",
            &[
                "kit/License.txt",
                "kit/Extra/License.txt",
                "kit/Extra/sub/a.png",
                "kit/Other/b.png",
            ],
        )
        .await;
        // The pass makes only the outermost marked folder a pack. A pack inside a pack comes from
        // a "Mark as pack" override, so the inner one is marked here by hand.
        sqlx::query("UPDATE folders SET is_pack = 1 WHERE path = 'kit/Extra'")
            .execute(&library.pool)
            .await
            .unwrap();
        for (path, pack) in [
            ("kit/Extra", "kit/Extra"),
            ("kit/Extra/sub", "kit/Extra"),
            ("kit/Other", "kit"),
            ("kit", "kit"),
        ] {
            let detail = pack_holding(&library, path).await.unwrap();
            assert_eq!(detail.map(|d| d.path).as_deref(), Some(pack), "{path}");
        }
    }

    /// One open library `lib` over the Car Kit, below `/libraries/lib`.
    async fn edited_car_kit() -> (Database, LibraryHandle) {
        let mut lib = memory_library("lib", crate::structure::tests::car_kit().await);
        lib.root_path = "/libraries/lib".into();
        (memory_database(vec![lib.clone()]).await, lib)
    }

    /// The node at `path` in the tree, read as the sidebar reads its parent's children.
    async fn tree_node(lib: &LibraryHandle, path: &str) -> FolderNode {
        let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
        child_folders(&lib.pool, &lib.root_path, parent)
            .await
            .unwrap()
            .into_iter()
            .find(|n| n.path == path)
            .expect("the node is in the tree")
    }

    #[tokio::test]
    async fn pack_edits_show_in_the_tree_the_packs_view_and_the_panel() {
        let (db, lib) = edited_car_kit().await;
        let car = "3D assets/Car Kit";
        let models = "3D assets/Car Kit/Models";
        let holiday = "3D assets/Holiday Kit";
        let edit = |path: &'static str, edit: PackEdit| {
            let db = &db;
            async move { set_pack_override_in(db, "lib", path, edit).await.unwrap() }
        };
        let mark = |mark| PackEdit::Mark { mark };
        let cards = || async {
            let cards = packs_below(std::slice::from_ref(&lib), "", false)
                .await
                .unwrap();
            cards
                .into_iter()
                .map(|c| (c.path, c.name, c.mark))
                .collect::<Vec<_>>()
        };

        edit(models, mark(Some(PackMark::Pack))).await;
        edit(holiday, mark(Some(PackMark::NotPack))).await;
        let node = tree_node(&lib, models).await;
        assert_eq!((node.is_pack, node.mark), (true, Some(PackMark::Pack)));
        let node = tree_node(&lib, holiday).await;
        assert_eq!((node.is_pack, node.mark), (false, Some(PackMark::NotPack)));
        assert_eq!(
            cards().await,
            [
                (car.into(), "Car Kit".into(), None),
                (models.into(), "Models".into(), Some(PackMark::Pack)),
            ]
        );

        let details = PackEdit::Details {
            name: Some("  My cars ".into()),
            license: Some("CC0, credit Kenney".into()),
            source_url: Some("https://kenney.nl/assets/car-kit".into()),
        };
        edit(car, details).await;
        let cover = PackEdit::Cover {
            asset_id: Some(SEDAN_RENDER.into()),
        };
        edit(car, cover).await;
        let render = format!("/libraries/lib/{SEDAN_RENDER}");
        let shown = |cover: Option<Cover>| cover.map(|c| c.file_path);
        let node = tree_node(&lib, car).await;
        assert_eq!(
            (node.name.as_str(), node.renamed.as_deref()),
            ("Car Kit", Some("My cars"))
        );
        assert_eq!(shown(node.cover), Some(render.clone()));
        // A name for a folder that is no pack shows nowhere.
        let named = PackEdit::Details {
            name: Some("Hols".into()),
            license: None,
            source_url: None,
        };
        edit(holiday, named).await;
        assert_eq!(tree_node(&lib, holiday).await.renamed, None);
        let card = packs_below(std::slice::from_ref(&lib), "", false)
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.path == car)
            .unwrap();
        assert_eq!(
            (card.name.as_str(), shown(card.cover)),
            ("My cars", Some(render.clone()))
        );
        let mut detail = pack_holding(&lib, car).await.unwrap().unwrap();
        assert_eq!(
            (detail.name.as_str(), shown(detail.cover.take())),
            ("My cars", Some(render))
        );
        assert_eq!(
            detail.edits,
            PackOverride {
                mark: None,
                name: Some("My cars".into()),
                license: Some("CC0, credit Kenney".into()),
                source_url: Some("https://kenney.nl/assets/car-kit".into()),
                cover_path: Some(SEDAN_RENDER.into()),
            }
        );

        // A refresh keeps every edit.
        structure::rebuild_library(&db, &lib).await;
        let mut again = pack_holding(&lib, car).await.unwrap().unwrap();
        assert!(again.cover.take().is_some());
        assert_eq!(again, detail);

        // Clearing every edit removes its rows and gives back the pass's own structure.
        let clear = PackEdit::Details {
            name: Some(" ".into()),
            license: None,
            source_url: Some(String::new()),
        };
        edit(car, clear).await;
        edit(car, PackEdit::Cover { asset_id: None }).await;
        edit(models, mark(None)).await;
        edit(holiday, mark(None)).await;
        edit(
            holiday,
            PackEdit::Details {
                name: None,
                license: None,
                source_url: None,
            },
        )
        .await;
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pack_overrides")
            .fetch_one(&lib.pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
        assert_eq!(
            crate::structure::tests::derived_rows(&lib.pool).await,
            crate::structure::tests::derived_rows(&crate::structure::tests::car_kit().await).await
        );
    }

    #[tokio::test]
    async fn pack_edits_refuse_an_unknown_folder_a_bad_url_and_a_cover_outside_the_pack() {
        let (db, lib) = edited_car_kit().await;
        let car = "3D assets/Car Kit";
        let details = |url: &str| PackEdit::Details {
            name: Some("x".into()),
            license: None,
            source_url: Some(url.into()),
        };
        let cover = |id: &str| PackEdit::Cover {
            asset_id: Some(id.into()),
        };
        let refusals = [
            ("lib", "3D assets/Bus Kit", details("https://kenney.nl")),
            ("lib", car, details("javascript:alert(1)")),
            ("lib", car, details("file:///etc/passwd")),
            ("lib", car, details("https://")),
            ("lib", car, cover(SEDAN[1])),
            ("lib", "3D assets/Holiday Kit", cover(SEDAN_RENDER)),
            ("lib", car, cover("no-such-asset")),
            ("closed", car, details("https://kenney.nl")),
        ];
        for (library, path, edit) in refusals {
            let refused = format!("{library} {path} {edit:?}");
            assert!(
                set_pack_override_in(&db, library, path, edit)
                    .await
                    .is_err(),
                "{refused}"
            );
        }
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pack_overrides")
            .fetch_one(&lib.pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn an_image_can_be_the_cover_of_the_innermost_pack_holding_it() {
        let (db, _lib) = edited_car_kit().await;
        let texture = "3D assets/Car Kit/Models/GLB format/Textures/colormap.png";
        let pack = |id: &'static str| {
            let db = &db;
            async move { cover_pack(db, id).await.unwrap().map(|p| (p.path, p.name)) }
        };
        assert_eq!(
            pack(texture).await,
            Some(("3D assets/Car Kit".into(), "Car Kit".into()))
        );
        // No model is a cover, and an unknown asset has no pack.
        assert_eq!(pack(SEDAN[1]).await, None);
        assert_eq!(pack("no-such-asset").await, None);
        let nest = PackEdit::Mark {
            mark: Some(PackMark::Pack),
        };
        set_pack_override_in(&db, "lib", "3D assets/Car Kit/Models", nest)
            .await
            .unwrap();
        assert_eq!(
            pack(texture).await,
            Some(("3D assets/Car Kit/Models".into(), "Models".into()))
        );
    }

    /// One open library over the Car Kit (`structure::tests::car_kit`, each asset's id its path),
    /// with a tag `t` and a collection `c` in master alone.
    async fn car_kit_database() -> (Database, Pool<Sqlite>) {
        let pool = crate::structure::tests::car_kit().await;
        let db = memory_database(vec![memory_library("lib", pool.clone())]).await;
        insert_tag(&db.master_pool, "t", "vehicle").await;
        insert_collection(&db.master_pool, "c", "cars", None).await;
        (db, pool)
    }

    /// The assets holding tag `t` and those in collection `c`, each sorted.
    async fn tagged_and_collected(pool: &Pool<Sqlite>) -> (Vec<String>, Vec<String>) {
        let read = |sql: &'static str| async move {
            sqlx::query_scalar::<_, String>(sql)
                .fetch_all(pool)
                .await
                .unwrap()
        };
        (
            read("SELECT asset_id FROM asset_tags WHERE tag_id = 't' ORDER BY 1").await,
            read("SELECT asset_id FROM collection_assets WHERE collection_id = 'c' ORDER BY 1")
                .await,
        )
    }

    /// Runs both bulk actions on `folder` of library `lib`, checking each returns `expected`'s
    /// length, and that they reached exactly `expected`.
    async fn assert_both_reach(db: &Database, folder: &str, mut expected: Vec<String>) {
        let pool = db.get_library_handle("lib").await.unwrap().pool;
        assert_eq!(
            tag_folder_in(db, "lib", folder, "t").await,
            Ok(expected.len())
        );
        assert_eq!(
            collect_folder_in(db, "lib", folder, "c").await,
            Ok(expected.len())
        );
        expected.sort();
        assert_eq!(
            tagged_and_collected(&pool).await,
            (expected.clone(), expected),
            "{folder:?}"
        );
    }

    async fn ids_where(pool: &Pool<Sqlite>, condition: &str) -> Vec<String> {
        sqlx::query_scalar(&format!("SELECT id FROM assets WHERE {condition}"))
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn bulk_actions_on_a_format_folder_reach_its_groups_members_in_sibling_folders() {
        let (db, _) = car_kit_database().await;
        let mut expected: Vec<String> = SEDAN.map(String::from).to_vec();
        expected.push("3D assets/Car Kit/Models/GLB format/Textures/colormap.png".into());

        assert_both_reach(&db, "3D assets/Car Kit/Models/GLB format/", expected).await;
    }

    #[tokio::test]
    async fn bulk_actions_on_a_pack_reach_every_file_in_it_support_files_included() {
        let (db, pool) = car_kit_database().await;
        let expected = ids_where(&pool, "file_path LIKE '3D assets/Car Kit/%'").await;
        assert_eq!(expected.len(), 8);
        assert!(expected.contains(&SEDAN_RENDER.to_string()));

        assert_both_reach(&db, "3D assets/Car Kit", expected).await;
    }

    #[tokio::test]
    async fn bulk_actions_on_the_library_root_reach_every_file_in_the_library() {
        let (db, pool) = car_kit_database().await;
        let expected = ids_where(&pool, "1").await;
        assert_eq!(expected.len(), 9);

        assert_both_reach(&db, "", expected).await;
    }

    #[tokio::test]
    async fn bulk_actions_copy_the_definitions_into_the_mirror_with_their_ancestors() {
        // Foreign keys on: a membership whose tag or collection the mirror lacks is refused.
        let pool = memory_library_pool_enforcing_foreign_keys().await;
        insert_asset(&pool, "rock").await;
        let db = memory_database(vec![memory_library("lib", pool.clone())]).await;
        insert_tag(&db.master_pool, "nature", "nature").await;
        insert_tag(&db.master_pool, "t", "stone").await;
        sqlx::query("UPDATE tags SET parent_id = 'nature' WHERE id = 't'")
            .execute(&db.master_pool)
            .await
            .unwrap();
        insert_collection(&db.master_pool, "props", "props", None).await;
        insert_collection(&db.master_pool, "c", "rocks", Some("props")).await;

        assert_eq!(tag_folder_in(&db, "lib", "", "t").await, Ok(1));
        assert_eq!(collect_folder_in(&db, "lib", "", "c").await, Ok(1));

        assert_eq!(
            tagged_and_collected(&pool).await,
            (vec!["rock".into()], vec!["rock".into()])
        );
        let mirrored: Vec<String> =
            sqlx::query_scalar("SELECT id FROM tags UNION ALL SELECT id FROM collections")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(mirrored.len(), 4);
    }

    #[tokio::test]
    async fn bulk_actions_write_past_one_batch_and_leave_other_libraries_alone() {
        let paths: Vec<String> = (0..450).map(|i| format!("big/{i}.png")).collect();
        let big = library(&paths.iter().map(String::as_str).collect::<Vec<_>>()).await;
        let other = library(&["big/0.png"]).await;
        let db = memory_database(vec![
            memory_library("lib", big.clone()),
            memory_library("other", other.clone()),
        ])
        .await;
        insert_tag(&db.master_pool, "t", "vehicle").await;
        insert_collection(&db.master_pool, "c", "cars", None).await;

        assert_both_reach(&db, "big", paths).await;
        assert_eq!(tagged_and_collected(&other).await, (vec![], vec![]));
    }

    #[test]
    fn a_batch_never_splits_a_group_that_reaches_past_its_size() {
        // 199 files on their own, then a group of 3 straddling the 200th row, then 300 more.
        let mut files: Vec<FileOfItem> = (0..199)
            .map(|i| (format!("a{i:03}"), format!("a{i:03}")))
            .collect();
        files.extend((0..3).map(|i| (format!("b{i}"), "b0".to_string())));
        files.extend((0..300).map(|i| (format!("c{i:03}"), format!("c{i:03}"))));

        let batches = group_batches(&files);

        let sizes: Vec<usize> = batches.iter().map(|b| b.len()).collect();
        assert_eq!(sizes, [202, 200, 100]);
        assert!(batches[0].ends_with(&files[199..202]));
        assert_eq!(batches.concat(), files);
        assert!(group_batches(&[]).is_empty());
    }

    #[tokio::test]
    async fn a_folders_files_come_with_each_groups_members_side_by_side() {
        let (_, pool) = car_kit_database().await;
        for folder in ["", "3D assets/Car Kit/Models/OBJ format"] {
            let files = folder_members(&pool, folder).await.unwrap();
            let at = files.iter().position(|(_, item)| item == SEDAN[1]).unwrap();
            let mut sedan: Vec<&str> = files[at..at + 3]
                .iter()
                .map(|(id, _)| id.as_str())
                .collect();
            sedan.sort();
            assert_eq!(sedan, SEDAN, "{folder:?}");
        }
    }

    #[tokio::test]
    async fn a_tag_or_collection_deleted_while_a_bulk_write_waits_does_not_come_back() {
        for collect in [false, true] {
            let (db, pool) = car_kit_database().await;
            let (id, read) = if collect {
                ("c", "SELECT COUNT(*) FROM collections WHERE id = 'c'")
            } else {
                ("t", "SELECT COUNT(*) FROM tags WHERE id = 't'")
            };
            // An index run holds the lock while the write starts, and the definition goes meanwhile.
            let lock = db.index_lock("lib");
            let guard = lock.lock().await;
            let write = async {
                if collect {
                    collect_folder_in(&db, "lib", "", id).await
                } else {
                    tag_folder_in(&db, "lib", "", id).await
                }
            };
            let delete = async {
                if collect {
                    crate::commands::collection::delete_collection_in(&db, id).await
                } else {
                    crate::commands::tag::delete_tag_in(&db, id).await
                }
                .unwrap();
                drop(guard);
            };
            let (written, ()) = tokio::join!(write, delete);

            let noun = if collect { "collection" } else { "tag" };
            assert_eq!(written, Err(format!("The {noun} no longer exists")));
            for p in [&db.master_pool, &pool] {
                let count: i64 = sqlx::query_scalar(read).fetch_one(p).await.unwrap();
                assert_eq!(count, 0, "{noun}");
            }
            assert_eq!(tagged_and_collected(&pool).await, (vec![], vec![]));
        }
    }

    #[tokio::test]
    async fn a_batch_and_a_definition_delete_never_overlap() {
        let (db, pool) = car_kit_database().await;
        let wait = std::time::Duration::from_millis(100);

        // While a batch holds the catalog lock, neither a delete nor another batch goes ahead.
        let held = db.catalog_lock.lock().await;
        let delete = tokio::time::timeout(wait, crate::commands::tag::delete_tag_in(&db, "t"));
        assert!(
            delete.await.is_err(),
            "the delete went ahead during a batch"
        );
        let write = tokio::time::timeout(wait, tag_folder_in(&db, "lib", "", "t"));
        assert!(write.await.is_err(), "a batch went ahead during another");
        assert_eq!(tagged_and_collected(&pool).await, (vec![], vec![]));
        drop(held);

        assert_eq!(tag_folder_in(&db, "lib", "", "t").await, Ok(9));
        crate::commands::tag::delete_tag_in(&db, "t").await.unwrap();
        let mirrored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tags WHERE id = 't'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mirrored, 0);
    }

    #[tokio::test]
    async fn bulk_actions_refuse_an_unknown_tag_or_collection_and_a_closed_library() {
        let (db, pool) = car_kit_database().await;

        assert_eq!(
            tag_folder_in(&db, "lib", "", "gone").await,
            Err("The tag no longer exists".into())
        );
        assert_eq!(
            collect_folder_in(&db, "lib", "", "gone").await,
            Err("The collection no longer exists".into())
        );
        assert_eq!(
            tag_folder_in(&db, "closed", "", "t").await,
            Err("Library closed is not open".into())
        );
        assert_eq!(tagged_and_collected(&pool).await, (vec![], vec![]));
    }
}
