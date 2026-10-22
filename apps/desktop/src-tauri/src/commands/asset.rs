use serde_json;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::{Emitter, State};

use crate::db::{make_relative_path, resolve_path, upsert_tag, Database, LibraryHandle};
use crate::models::{
    Asset, AssetPatch, FilterSpec, FolderRef, GroupMember, SearchPayload, SearchQuery,
    SearchResult, SortSpec,
};
use crate::structure;
use crate::AppState;

type Result<T> = std::result::Result<T, String>;

/// A read transaction on one library's pool.
type LibraryTx = sqlx::Transaction<'static, sqlx::Sqlite>;

#[derive(Debug, Clone, sqlx::FromRow)]
struct RawAssetRow {
    pub id: String,
    pub file_path: String,
    pub file_name: String,
    pub extension: String,
    pub kind: String,
    pub size_bytes: i64,
    pub modified_at: i64,
    pub indexed_at: i64,
    pub rating: i64,
    pub color_label: String,
    pub is_favorite: i64,
    pub description: String,
    pub thumbnail_path: Option<String>,
    pub preview_status: String,
    pub metadata: String,
    pub tags: Option<String>,
    pub collections: Option<String>,
    /// A main item's members' extensions, as a JSON array, itself included
    pub group_formats: Option<String>,
    /// The picture of an item's render that fits a card: its thumbnail, or the render itself
    pub bundled_preview: Option<String>,
    /// The picture of an item's render that is too small to fit a card, as `bundled_preview`
    pub fallback_render: Option<String>,
}

/// Build an Asset from a raw SQLite row, resolving relative paths to absolute paths
fn row_to_asset(root: Option<&Path>, row: RawAssetRow) -> Asset {
    let mut metadata: std::collections::HashMap<String, serde_json::Value> =
        serde_json::from_str(&row.metadata).unwrap_or_default();
    if let (Some(r), Some(pm)) = (root, metadata.get("preview_model").and_then(|v| v.as_str())) {
        let abs_pm = resolve_path(r, pm);
        metadata.insert(
            "preview_model".to_string(),
            serde_json::Value::String(abs_pm),
        );
    }
    let tags: Vec<String> = row
        .tags
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let collections: Vec<String> = row
        .collections
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let group_formats = row
        .group_formats
        .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
        .map(|formats| main_first(&row.extension, formats));

    let resolved_file_path = match root {
        Some(r) => resolve_path(r, &row.file_path),
        None => row.file_path,
    };
    let resolve = |path: Option<String>| match (root, path) {
        (Some(r), Some(p)) => Some(resolve_path(r, &p)),
        (_, p) => p,
    };
    let resolved_thumb_path = resolve(row.thumbnail_path);
    let bundled_preview = resolve(row.bundled_preview);
    let fallback_render = resolve(row.fallback_render);

    Asset {
        id: row.id,
        file_path: resolved_file_path,
        file_name: row.file_name,
        extension: row.extension,
        kind: row.kind,
        size_bytes: row.size_bytes,
        modified_at: row.modified_at,
        indexed_at: row.indexed_at,
        rating: row.rating,
        color_label: row.color_label,
        is_favorite: row.is_favorite != 0,
        description: row.description,
        tags,
        collections,
        thumbnail_path: resolved_thumb_path,
        preview_status: row.preview_status,
        metadata,
        group_formats,
        bundled_preview,
        fallback_render,
    }
}

/// A group's member extensions with the main item's (`main`) first, then the rest in the order
/// the group picks its main item by (`structure::format_rank`).
fn main_first(main: &str, mut formats: Vec<String>) -> Vec<String> {
    if let Some(i) = formats.iter().position(|f| f == main) {
        formats.remove(i);
    }
    formats.sort_by(|a, b| {
        structure::format_rank(a)
            .cmp(&structure::format_rank(b))
            .then_with(|| a.cmp(b))
    });
    formats.insert(0, main.to_string());
    formats
}

/// Builds an FTS5 `MATCH` expression from free search text. Each whitespace-separated term
/// becomes a quoted FTS5 string (embedded `"` doubled) with a prefix `*`, so input like
/// `low-poly`, `tree.glb` or `AND` is matched literally instead of parsed as FTS5 syntax.
/// Terms with no letters or digits (`"`, `(`) would tokenize to nothing and match no row, so
/// they are dropped; `None` means there is nothing to match on.
fn fts_match_expression(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split_whitespace()
        .filter(|t| t.chars().any(char::is_alphanumeric))
        .map(|t| format!("\"{}\"*", t.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

#[tauri::command]
pub async fn search_assets(
    payload: SearchPayload,
    state: State<'_, Arc<AppState>>,
) -> Result<SearchResult> {
    let all_libs = state.db.get_all_libraries().await;
    let target_libs = target_libraries(&payload.query, &all_libs);
    search_libraries(&payload, &target_libs).await
}

/// The pools and roots of the libraries `query` searches, in `all_libs` order. A folder searches
/// only its own library, and nothing when that library isn't open. Otherwise the query's
/// libraries, or every library when it names none that is open.
fn target_libraries(
    query: &SearchQuery,
    all_libs: &[LibraryHandle],
) -> Vec<(sqlx::Pool<sqlx::Sqlite>, Option<std::path::PathBuf>)> {
    let target = |lib: &LibraryHandle| (lib.pool.clone(), Some(lib.root_path.clone()));
    if let Some(folder) = &query.folder {
        return all_libs
            .iter()
            .filter(|lib| lib.id == folder.library_id)
            .map(target)
            .collect();
    }
    let mut list = Vec::new();
    if let Some(ref lids) = query.library_ids {
        if !lids.is_empty() {
            for lib in all_libs {
                if lids.contains(&lib.id) {
                    list.push(target(lib));
                }
            }
        }
    } else if let Some(ref lid) = query.library_id {
        if !lid.is_empty() {
            for lib in all_libs {
                if &lib.id == lid {
                    list.push(target(lib));
                }
            }
        }
    }

    if list.is_empty() {
        list = all_libs.iter().map(target).collect();
    }
    list
}

async fn search_libraries(
    payload: &SearchPayload,
    target_libs: &[(sqlx::Pool<sqlx::Sqlite>, Option<std::path::PathBuf>)],
) -> Result<SearchResult> {
    let offset = payload.page * payload.page_size;
    let limit = payload.page_size;
    let split = folder_splits_groups(&payload.query, target_libs)
        .await
        .map_err(|e| e.to_string())?;
    let filter = search_filter(&payload.query, split);
    let sort_field = payload.query.sort.field.as_str();
    let is_asc = payload.query.sort.order == "asc";
    let order_by = order_by(sort_field, is_asc);

    if target_libs.len() == 1 {
        let (pool, root_opt) = &target_libs[0];
        let counts = if payload.include_total {
            Some(
                count_assets(pool, &filter)
                    .await
                    .map_err(|e| e.to_string())?,
            )
        } else {
            None
        };
        let assets = fetch_assets(pool, root_opt, &filter, &order_by, limit, Some(offset))
            .await
            .map_err(|e| e.to_string())?;
        return Ok(SearchResult {
            assets,
            total: counts.map(|c| c.items),
            file_total: counts.map(|c| c.files),
            page: payload.page,
            page_size: payload.page_size,
        });
    }

    // Multiple libraries: each returns only the rowid and sort key of its first `offset + limit`
    // matches, and only the merged page's rows are fetched in full.
    let mut total = Counts::default();
    if payload.include_total {
        for (pool, _) in target_libs {
            if let Ok(c) = count_assets(pool, &filter).await {
                total.items += c.items;
                total.files += c.files;
            }
        }
    }
    // Each library's keys and its page rows are read in one transaction, one snapshot, so an
    // asset the watcher deletes in between can't drop out of the page, nor a new asset that
    // reuses its rowid take its place. Under rollback journaling, writers wait for it to end.
    let mut txs: Vec<Option<LibraryTx>> = Vec::new();
    for (lib, (pool, _)) in target_libs.iter().enumerate() {
        match pool.begin().await {
            Ok(tx) => txs.push(Some(tx)),
            Err(e) => {
                tracing::warn!("Left library {lib} out of a search: {e}");
                txs.push(None);
            }
        }
    }
    let (libs, f) = (&mut txs, &filter);
    let page = match sort_column(sort_field).1 {
        SortKind::NoCase => {
            merged_page::<String>(libs, f, sort_field, is_asc, offset, limit, |a, b| {
                nocase_cmp(a, b)
            })
            .await
        }
        SortKind::Binary => {
            merged_page::<String>(libs, f, sort_field, is_asc, offset, limit, |a, b| {
                a.as_bytes().cmp(b.as_bytes())
            })
            .await
        }
        SortKind::Integer => {
            merged_page::<i64>(libs, f, sort_field, is_asc, offset, limit, i64::cmp).await
        }
    };

    #[cfg(test)]
    if let Some(hook) = BETWEEN_KEYS_AND_ROWS.with(|h| h.borrow_mut().take()) {
        hook();
    }
    let mut fetched: HashMap<(usize, i64), Asset> = HashMap::new();
    for (lib, (tx, (_, root_opt))) in txs.iter_mut().zip(target_libs).enumerate() {
        let Some(tx) = tx else { continue };
        let rowids: Vec<i64> = page
            .iter()
            .filter(|(l, _)| *l == lib)
            .map(|(_, rowid)| *rowid)
            .collect();
        if rowids.is_empty() {
            continue;
        }
        match fetch_assets_by_rowid(tx, root_opt, &rowids).await {
            Ok(assets) => fetched.extend(
                assets
                    .into_iter()
                    .map(|(rowid, asset)| ((lib, rowid), asset)),
            ),
            Err(e) => tracing::warn!("Left library {lib} out of a search page: {e}"),
        }
    }
    for tx in txs.into_iter().flatten() {
        // Ending the read transaction now releases its lock instead of leaving it on the idle
        // connection.
        if let Err(e) = tx.commit().await {
            tracing::warn!("Could not end a search's read transaction: {e}");
        }
    }
    let assets = page.iter().filter_map(|key| fetched.remove(key)).collect();

    Ok(SearchResult {
        assets,
        total: payload.include_total.then_some(total.items),
        file_total: payload.include_total.then_some(total.files),
        page: payload.page,
        page_size: payload.page_size,
    })
}

/// The (library position, rowid) of each asset on the page `offset..offset + limit` of a search
/// over the libraries read by `txs` sorted by `field`, in the order one SQLite query over all of
/// them would return. Each library returns only the rowid and sort key of its first `offset + limit` matches,
/// whose keys `cmp` compares as SQLite does. Ties break by library position, then rowid.
async fn merged_page<K>(
    txs: &mut [Option<LibraryTx>],
    filter: &SqlFilter,
    field: &str,
    is_asc: bool,
    offset: i64,
    limit: i64,
    cmp: fn(&K, &K) -> Ordering,
) -> Vec<(usize, i64)>
where
    K: Send + Unpin + for<'r> sqlx::Decode<'r, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
{
    let sql = format!(
        "SELECT a.rowid, {} FROM assets a {} ORDER BY {} LIMIT ?",
        sort_column(field).0,
        filter.where_clause(),
        order_by(field, is_asc),
    );
    let mut keys: Vec<(usize, i64, K)> = Vec::new();
    for (lib, tx) in txs.iter_mut().enumerate() {
        let Some(tx) = tx else { continue };
        let mut query = sqlx::query_as::<_, (i64, K)>(&sql);
        for b in filter.binds() {
            query = query.bind(b.clone());
        }
        match query.bind(offset + limit).fetch_all(&mut **tx).await {
            Ok(rows) => keys.extend(rows.into_iter().map(|(rowid, key)| (lib, rowid, key))),
            Err(e) => tracing::warn!("Left library {lib} out of a search: {e}"),
        }
    }

    // Each library's keys are already in its order, so the stable sort merges sorted runs.
    keys.sort_by(|(lib_a, rowid_a, key_a), (lib_b, rowid_b, key_b)| {
        let by_key = cmp(key_a, key_b);
        let by_key = if is_asc { by_key } else { by_key.reverse() };
        let by_rowid = if is_asc {
            rowid_a.cmp(rowid_b)
        } else {
            rowid_b.cmp(rowid_a)
        };
        by_key.then(lib_a.cmp(lib_b)).then(by_rowid)
    });
    keys.into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .map(|(lib, rowid, _)| (lib, rowid))
        .collect()
}

/// Compares text as SQLite's `NOCASE` collation does: ASCII letters fold to lowercase, then the
/// bytes compare, a prefix first.
fn nocase_cmp(a: &str, b: &str) -> Ordering {
    a.bytes()
        .map(|c| c.to_ascii_lowercase())
        .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

/// The `ORDER BY` terms of a search over `assets a` sorted by `field`. The `rowid` tie-break makes
/// the order total, so offset pages never overlap or skip.
fn order_by(field: &str, is_asc: bool) -> String {
    let dir = if is_asc { "ASC" } else { "DESC" };
    format!("{} {dir}, a.rowid {dir}", sort_column(field).0)
}

/// How SQLite compares a sort column's values, and so how the multi-library merge decodes and
/// compares them.
#[derive(Clone, Copy)]
enum SortKind {
    /// Text under `COLLATE NOCASE`.
    NoCase,
    /// Text under the default `BINARY` collation.
    Binary,
    Integer,
}

/// The column of `assets a` a search sorted by `field` orders on, with its collation, and how
/// its values compare.
fn sort_column(field: &str) -> (&'static str, SortKind) {
    match field {
        "name" => ("a.file_name COLLATE NOCASE", SortKind::NoCase),
        "size" => ("a.size_bytes", SortKind::Integer),
        "rating" => ("a.rating", SortKind::Integer),
        "kind" => ("a.kind", SortKind::Binary),
        "favorite" => ("a.is_favorite", SortKind::Integer),
        _ => ("a.modified_at", SortKind::Integer),
    }
}

/// The `WHERE` conditions of a search over `assets a`, and the values bound to their `?`
/// placeholders in order.
///
/// A search lists grid items: main items of format groups and files on their own (plan C5). The
/// item conditions read the item itself: its metadata, and the size and date that sort it. The
/// file-level conditions (plan C6: search text, kind, extension, folder, name prefix and suffix)
/// match an item when one of its files, the main item included, meets them all together.
#[derive(Default)]
struct SqlFilter {
    conditions: Vec<String>,
    binds: Vec<String>,
    /// Written on the file `{f}`, so they read any file of an item, or the file itself.
    file_conditions: Vec<String>,
    file_binds: Vec<String>,
    /// Whether an item can match through a file other than its main item. Otherwise every
    /// file-level condition is a folder range the item conditions hold too.
    any_file: bool,
    /// Whether support files match too ("Show hidden files"). Otherwise the items leave them out
    /// with `NOT_SUPPORT`, while the files behind the items count them either way.
    show_hidden: bool,
}

/// The group half of the grid's visibility rule (plan C5): a group shows as its main item, and
/// its other members never show, even with "Show hidden files". With both halves, a search reads
/// the grid items from `idx_assets_items`, whose `WHERE` repeats them term for term
/// (`db::add_structure_columns`). The preview queue claims jobs by it too (`preview::claim_jobs`),
/// on `assets a`.
pub(crate) const MAIN_OR_SINGLE: &str = "(a.group_id IS NULL OR a.group_id = a.id)";

/// Whether item `item` (an alias of `assets`) is pictured: its `bundled_preview` names a render
/// that fits a card, both of its cached sides at least `THUMB_SIZE`. A render of unknown size
/// doesn't fit. It is `structure::Structure::pictured` in SQL (plan small-bundled-previews C1),
/// and only a pictured item stays off the preview queue (`preview::claim_jobs`). Its render's
/// cached size is current: the pass writes it before the `bundled_preview` that names it.
pub(crate) fn pictured(item: &str) -> String {
    let side = crate::preview::THUMB_SIZE;
    format!(
        "EXISTS (SELECT 1 FROM assets r WHERE r.file_path = {item}.bundled_preview
             AND r.render_width >= {side} AND r.render_height >= {side})"
    )
}

/// The support-file half of the grid's visibility rule: a pack's support files show only with
/// "Show hidden files".
const NOT_SUPPORT: &str = "a.support_role IS NULL";

impl SqlFilter {
    fn push(&mut self, condition: impl Into<String>, binds: impl IntoIterator<Item = String>) {
        self.conditions.push(condition.into());
        self.binds.extend(binds);
    }

    /// Requires `column` to be one of `values`, unless there are none.
    fn push_in(&mut self, column: &str, values: &Option<Vec<String>>) {
        if let Some(values) = values.as_ref().filter(|v| !v.is_empty()) {
            self.push(
                format!("{} IN ({})", column, placeholders(values.len())),
                values.iter().cloned(),
            );
        }
    }

    /// Adds a file-level condition, written on `{f}`.
    fn push_file(&mut self, condition: impl Into<String>, binds: impl IntoIterator<Item = String>) {
        self.any_file = true;
        self.push_implied_file(condition, binds);
    }

    /// Adds a file-level condition the item conditions already hold for the main item.
    fn push_implied_file(
        &mut self,
        condition: impl Into<String>,
        binds: impl IntoIterator<Item = String>,
    ) {
        self.file_conditions.push(condition.into());
        self.file_binds.extend(binds);
    }

    /// Requires a file's `column` (on `{f}`) to be one of `values`, unless there are none.
    fn push_file_in(&mut self, column: &str, values: &Option<Vec<String>>) {
        if let Some(values) = values.as_ref().filter(|v| !v.is_empty()) {
            self.push_file(
                format!("{} IN ({})", column, placeholders(values.len())),
                values.iter().cloned(),
            );
        }
    }

    /// The file-level conditions on the file `alias`, joined by `AND`.
    fn file_conditions_on(&self, alias: &str) -> String {
        self.file_conditions
            .iter()
            .map(|c| c.replace("{f}", alias))
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    /// The grid items: every condition, with the visibility rule. An item matches the file-level
    /// conditions through any of its files `m`: `m.id = a.id` for a file on its own, and
    /// `m.group_id = a.id` for each member of a group, its main item included.
    fn where_clause(&self) -> String {
        let mut all: Vec<String> = self.conditions.clone();
        all.push(MAIN_OR_SINGLE.to_string());
        if !self.show_hidden {
            all.push(NOT_SUPPORT.to_string());
        }
        if self.any_file {
            all.push(format!(
                "a.id IN (SELECT COALESCE(m.group_id, m.id) FROM assets m WHERE {})",
                self.file_conditions_on("m")
            ));
        }
        format!("WHERE {}", all.join(" AND "))
    }

    /// The values bound to `where_clause`'s placeholders, in order.
    fn binds(&self) -> impl Iterator<Item = &String> {
        let file_binds = if self.any_file {
            &self.file_binds[..]
        } else {
            &[]
        };
        self.binds.iter().chain(file_binds)
    }

    /// The values bound to `files_where_clause`'s placeholders, in order, and to `grouped_sql`'s
    /// before its `extra` ones.
    fn files_binds(&self) -> impl Iterator<Item = &String> {
        self.binds.iter().chain(&self.file_binds)
    }

    /// The files behind the items: every file that meets every condition itself, members and
    /// support files included. Bound by `files_binds`.
    fn files_where_clause(&self) -> String {
        let file_conditions = self.file_conditions_on("a");
        let all: Vec<&str> = self
            .conditions
            .iter()
            .map(String::as_str)
            .chain((!file_conditions.is_empty()).then_some(file_conditions.as_str()))
            .collect();
        if all.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", all.join(" AND "))
        }
    }

    /// Counts the grid items per value of `key`, an expression on a file `m` (plan C23): an item
    /// counts once toward each value among its files that meet every file-level condition and
    /// `extra` (on `m`) together. So each count is `where_clause`'s total with the condition
    /// `key = value` added, and an item can count toward several values. Bound by `files_binds`,
    /// then the values `extra` binds.
    fn grouped_sql(&self, key: &str, extra: &[String]) -> String {
        let mut all: Vec<String> = self.conditions.clone();
        all.push(MAIN_OR_SINGLE.to_string());
        if !self.show_hidden {
            all.push(NOT_SUPPORT.to_string());
        }
        let file_conditions = self.file_conditions_on("m");
        if !file_conditions.is_empty() {
            all.push(file_conditions);
        }
        all.extend(extra.iter().cloned());
        format!(
            "SELECT {key} AS value, COUNT(DISTINCT a.id)
             FROM assets m JOIN assets a ON a.id = COALESCE(m.group_id, m.id)
             WHERE {}
             GROUP BY value",
            all.join(" AND ")
        )
    }
}

fn placeholders(count: usize) -> String {
    vec!["?"; count].join(",")
}

/// A condition matching assets in any of `count` collections (bound as `?`), and with
/// `include_sub`, in any of their nested subcollections.
fn in_collections(count: usize, include_sub: bool) -> String {
    let ids = placeholders(count);
    if include_sub {
        format!(
            "a.id IN (
                WITH RECURSIVE col_tree AS (
                    SELECT id FROM collections WHERE id IN ({ids})
                    UNION ALL
                    SELECT c.id FROM collections c JOIN col_tree ct ON c.parent_id = ct.id
                )
                SELECT asset_id FROM collection_assets WHERE collection_id IN (SELECT id FROM col_tree)
            )"
        )
    } else {
        format!("a.id IN (SELECT asset_id FROM collection_assets WHERE collection_id IN ({ids}))")
    }
}

/// A condition matching assets tagged with any of `count` tags (bound as `?`).
fn in_tags(count: usize) -> String {
    format!(
        "a.id IN (SELECT asset_id FROM asset_tags WHERE tag_id IN ({}))",
        placeholders(count)
    )
}

/// Whether folder `path` of the library at `pool` holds a member of a format group whose main
/// item lies outside it (`structure::Folder::split_groups`). The root holds every main item, and
/// a folder with no row holds no file.
async fn splits_groups(pool: &sqlx::Pool<sqlx::Sqlite>, path: &str) -> sqlx::Result<bool> {
    let path = path.trim_matches('/');
    if path.is_empty() {
        return Ok(false);
    }
    let split: Option<i64> = sqlx::query_scalar("SELECT split_groups FROM folders WHERE path = ?")
        .bind(path)
        .fetch_optional(pool)
        .await?;
    Ok(split.unwrap_or(0) > 0)
}

/// Whether the folder `query` names splits format groups in its library, the first of `targets`
/// (`target_libraries`). False when it names none.
async fn folder_splits_groups(
    query: &SearchQuery,
    targets: &[(sqlx::Pool<sqlx::Sqlite>, Option<std::path::PathBuf>)],
) -> sqlx::Result<bool> {
    match (&query.folder, targets.first()) {
        (Some(folder), Some((pool, _))) => splits_groups(pool, &folder.path).await,
        _ => Ok(false),
    }
}

/// The grid's items below `folder` of the library at `pool`, in file path order: the search the
/// grid runs with that folder selected and hidden files off, so main items and files on their
/// own, never support files (plan C22). With `untagged_only`, the grid's untagged filter keeps
/// the items with no tag. The root (`""`) covers the whole library. Auto-tag's folder and
/// library runs send these, so they stay what the grid and the folder's badge show.
pub(crate) async fn folder_item_ids(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    folder: &str,
    untagged_only: bool,
) -> sqlx::Result<Vec<String>> {
    let query = SearchQuery {
        text: String::new(),
        filter: FilterSpec {
            tag_logic: untagged_only.then(|| "none".to_string()),
            ..FilterSpec::default()
        },
        sort: SortSpec {
            field: "name".into(),
            order: "asc".into(),
        },
        library_id: None,
        library_ids: None,
        collection_id: None,
        include_subcollections: None,
        folder: Some(FolderRef {
            library_id: String::new(),
            path: folder.to_string(),
        }),
    };
    let filter = search_filter(&query, splits_groups(pool, folder).await?);
    let sql = format!(
        "SELECT a.id FROM assets a {} ORDER BY a.file_path",
        filter.where_clause()
    );
    let mut ids = sqlx::query_scalar(&sql);
    for bound in filter.binds() {
        ids = ids.bind(bound);
    }
    ids.fetch_all(pool).await
}

/// The search for `query`. `split` tells whether its folder splits format groups
/// (`splits_groups`).
fn search_filter(query: &SearchQuery, split: bool) -> SqlFilter {
    let f = &query.filter;
    let mut filter = SqlFilter {
        show_hidden: f.show_hidden == Some(true),
        ..SqlFilter::default()
    };

    if let Some(fts) = fts_match_expression(&query.text) {
        filter.push_file(
            "{f}.rowid IN (SELECT rowid FROM assets_fts WHERE assets_fts MATCH ?)",
            [fts],
        );
    }
    filter.push_file_in("{f}.kind", &f.kinds);
    filter.push_in("a.color_label", &f.color_labels);
    if let Some(rating) = &f.rating {
        filter.push(
            "a.rating >= ? AND a.rating <= ?",
            [rating.min.to_string(), rating.max.to_string()],
        );
    }
    if let Some(is_fav) = f.is_favorite {
        filter.push(format!("a.is_favorite = {}", is_fav as i32), []);
    }
    if let Some(dr) = &f.date_range {
        if let Some(from) = dr.from {
            filter.push("a.modified_at >= ?", [from.to_string()]);
        }
        if let Some(to) = dr.to {
            filter.push("a.modified_at <= ?", [to.to_string()]);
        }
    }
    if let Some(sr) = &f.size_range {
        if let Some(min) = sr.min {
            filter.push("a.size_bytes >= ?", [min.to_string()]);
        }
        if let Some(max) = sr.max {
            filter.push("a.size_bytes <= ?", [max.to_string()]);
        }
    }
    match f.preview_status.as_deref() {
        None | Some("") => {}
        Some("missing" | "needs_preview") => {
            filter.push("a.preview_status IN ('none', 'error')", [])
        }
        Some("has_preview") => filter.push("a.preview_status = 'done'", []),
        Some(ps) => filter.push("a.preview_status = ?", [ps.to_string()]),
    }
    filter.push_file_in("{f}.extension", &f.extensions);
    if let Some(folder) = &query.folder {
        push_folder_range(&mut filter, &folder.path, split);
    }
    push_tag_filter(&mut filter, f);
    push_collection_filter(&mut filter, query);

    // Smart filter conditions
    fn trimmed(s: &Option<String>) -> Option<&str> {
        s.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    // Typed by hand, so padding is dropped; a tree folder's path is used exactly. It applies to
    // every library searched, so it may split groups in any of them.
    if let Some(dir) = &f.directory {
        push_folder_range(&mut filter, dir.trim(), true);
    }
    if let Some(prefix) = trimmed(&f.name_prefix) {
        filter.push_file("{f}.file_name LIKE ?", [format!("{prefix}%")]);
    }
    if let Some(suffix) = trimmed(&f.name_suffix) {
        filter.push_file("{f}.file_name LIKE ?", [format!("%{suffix}")]);
    }
    filter
}

/// The `file_path` range of the files below `folder`, a path relative to the library root,
/// recursively: `p/ <= file_path < p0` (`0` follows `/`) holds exactly the paths starting with
/// `p/`, uses the unique `file_path` index and needs no `LIKE` escaping. `None` for the root
/// (`""`, or only slashes), which every file lies below. Spaces are kept: a folder name may start
/// or end with one. The grid's folder filter and the tree's live counts both read folders by it.
pub(crate) fn folder_range(folder: &str) -> Option<[String; 2]> {
    let path = folder.trim_matches('/');
    (!path.is_empty()).then(|| [format!("{path}/"), format!("{path}0")])
}

/// Requires files below `folder` (`folder_range`). The root adds no condition.
///
/// It is a file-level condition. When the folder `split`s no group, every group with a file
/// below it has its main item there too, so the item's own path decides, as fast as before
/// groups: the range goes on the item as well, and adds no search through members.
fn push_folder_range(filter: &mut SqlFilter, folder: &str, split: bool) {
    let Some(range) = folder_range(folder) else {
        return;
    };
    let condition = "{f}.file_path >= ? AND {f}.file_path < ?";
    if split {
        filter.push_file(condition, range);
    } else {
        filter.push(condition.replace("{f}", "a"), range.clone());
        filter.push_implied_file(condition, range);
    }
}

/// Tag filter: logic "and" (has all tags), "or" (has any tag), "and_or" (has the first tag
/// AND any of the rest), "none" (untagged).
fn push_tag_filter(filter: &mut SqlFilter, f: &FilterSpec) {
    let tags = f.tags.as_deref().unwrap_or_default();
    match f.tag_logic.as_deref().unwrap_or("or") {
        "none" => filter.push("a.id NOT IN (SELECT asset_id FROM asset_tags)", []),
        _ if tags.is_empty() => {}
        "and" => filter.push(
            format!(
                "a.id IN (SELECT asset_id FROM asset_tags WHERE tag_id IN ({}) GROUP BY asset_id HAVING COUNT(DISTINCT tag_id) = ?)",
                placeholders(tags.len())
            ),
            tags.iter().cloned().chain([tags.len().to_string()]),
        ),
        "and_or" => {
            let (primary, rest) = tags.split_first().unwrap();
            filter.push(in_tags(1), [primary.clone()]);
            if !rest.is_empty() {
                filter.push(in_tags(rest.len()), rest.iter().cloned());
            }
        }
        _ => filter.push(in_tags(tags.len()), tags.iter().cloned()),
    }
}

/// Collection filter over the query's collection and the filter's collections, each including
/// its subcollections unless turned off: logic "and" (in all), "or" (in any), "and_or" (in the
/// first AND any of the rest), "none" (unorganized).
fn push_collection_filter(filter: &mut SqlFilter, query: &SearchQuery) {
    let f = &query.filter;
    let mut targets: Vec<String> = Vec::new();
    let query_col = query.collection_id.as_ref().or(f.collection_id.as_ref());
    for cid in query_col
        .into_iter()
        .chain(f.collection_ids.iter().flatten())
    {
        if !cid.is_empty() && !targets.contains(cid) {
            targets.push(cid.clone());
        }
    }
    let include_sub = f
        .include_subcollections
        .unwrap_or_else(|| query.include_subcollections.unwrap_or(true));

    match f.collection_logic.as_deref().unwrap_or("or") {
        logic if logic == "none" || f.unorganized == Some(true) => {
            filter.push("a.id NOT IN (SELECT asset_id FROM collection_assets)", [])
        }
        _ if targets.is_empty() => {}
        "and" => {
            for cid in targets {
                filter.push(in_collections(1, include_sub), [cid]);
            }
        }
        "and_or" => {
            let (primary, rest) = targets.split_first().unwrap();
            filter.push(in_collections(1, include_sub), [primary.clone()]);
            if !rest.is_empty() {
                filter.push(
                    in_collections(rest.len(), include_sub),
                    rest.iter().cloned(),
                );
            }
        }
        _ => filter.push(in_collections(targets.len(), include_sub), targets),
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only: how many count queries this thread has started, so a test can tell a skipped
    /// count from one whose result was dropped.
    static COUNT_QUERIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    /// Test-only: runs once in the next multi-library search, after the keys are merged and
    /// before the page's rows are fetched.
    static BETWEEN_KEYS_AND_ROWS: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// A search's result count: the grid items, and the files they stand for, hidden support files
/// included.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Counts {
    items: i64,
    files: i64,
}

/// Counts the items and the files `filter` matches. With no condition but a folder, both come from
/// an index alone: the items from `idx_assets_items`, the files from the `file_path` index.
async fn count_assets(pool: &sqlx::Pool<sqlx::Sqlite>, filter: &SqlFilter) -> sqlx::Result<Counts> {
    #[cfg(test)]
    COUNT_QUERIES.with(|c| c.set(c.get() + 1));
    let sql = format!(
        "SELECT (SELECT COUNT(*) FROM assets a {}), (SELECT COUNT(*) FROM assets a {})",
        filter.where_clause(),
        filter.files_where_clause()
    );
    let mut query = sqlx::query_as::<_, (i64, i64)>(&sql);
    for b in filter.binds().chain(filter.files_binds()) {
        query = query.bind(b.clone());
    }
    let (items, files) = query.fetch_one(pool).await?;
    Ok(Counts { items, files })
}

/// The grid items per value of `key` that `filter` and `extra` match in the library at `pool`
/// (`SqlFilter::grouped_sql`), added into `counts`.
async fn add_grouped_counts(
    counts: &mut HashMap<String, i64>,
    pool: &sqlx::Pool<sqlx::Sqlite>,
    filter: &SqlFilter,
    key: &str,
    extra: &[String],
    extra_binds: &[String],
) -> sqlx::Result<()> {
    let sql = filter.grouped_sql(key, extra);
    let mut query = sqlx::query_as::<_, (String, i64)>(&sql);
    for b in filter.files_binds().chain(extra_binds) {
        query = query.bind(b.clone());
    }
    for (value, n) in query.fetch_all(pool).await? {
        *counts.entry(value).or_default() += n;
    }
    Ok(())
}

/// The live counts beside the sidebar's Types filter and the filter bar's format filter (plan
/// C23), by kind and by extension. Each is the grid's total once that value is picked: the
/// sidebar's Types replace the kind filter, and a format pill adds its extension to the active
/// ones.
#[derive(Debug, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FacetCounts {
    /// The grid's total with each kind as the only one, for the kinds that have one
    pub kinds: HashMap<String, i64>,
    /// The grid's total with each extension added to the active ones, for the extensions that
    /// change it or have one. Read through `extension`, which covers the rest.
    pub extensions: HashMap<String, i64>,
    /// The count of an extension `extensions` leaves out: the grid's total with the active
    /// extensions, which adding one no file has keeps, or 0 when none is active
    pub extension_base: i64,
}

impl FacetCounts {
    /// The grid's total once `extension` joins the active ones. Reads every extension count, as
    /// `extensions` holds only those that differ from `extension_base`'s.
    pub fn extension(&self, extension: &str) -> i64 {
        self.extensions
            .get(extension)
            .copied()
            .unwrap_or(self.extension_base)
    }
}

/// The kind and extension counts for `query` (`FacetCounts`). Counted once per query, like the
/// total.
#[tauri::command]
pub async fn get_facet_counts(
    query: SearchQuery,
    state: State<'_, Arc<AppState>>,
) -> Result<FacetCounts> {
    let all_libs = state.db.get_all_libraries().await;
    facet_counts(&query, &target_libraries(&query, &all_libs))
        .await
        .map_err(|e| e.to_string())
}

/// `get_facet_counts` over `targets`, the libraries `query` searches (`target_libraries`), each
/// adding its items. A kind's count leaves out the query's kind filter, which picking the kind
/// replaces. An extension's count leaves out the extension filter too, and with extensions
/// active, it is the items those match plus the items only that extension adds: the ones with a
/// file of that extension meeting every other file-level filter, and no file of an active
/// extension doing so.
async fn facet_counts(
    query: &SearchQuery,
    targets: &[(sqlx::Pool<sqlx::Sqlite>, Option<std::path::PathBuf>)],
) -> sqlx::Result<FacetCounts> {
    let split = folder_splits_groups(query, targets).await?;
    let mut any_kind = query.clone();
    any_kind.filter.kinds = None;
    let mut any_extension = query.clone();
    any_extension.filter.extensions = None;
    let (by_kind, by_extension) = (
        search_filter(&any_kind, split),
        search_filter(&any_extension, split),
    );
    let active = query
        .filter
        .extensions
        .as_ref()
        .is_some_and(|e| !e.is_empty())
        .then(|| search_filter(query, split));
    // The items the active extensions already match, which an added extension's count leaves out
    let (extra, extra_binds) = match &active {
        Some(with_active) => (
            vec![format!(
                "a.id NOT IN (SELECT COALESCE(s.group_id, s.id) FROM assets s WHERE {})",
                with_active.file_conditions_on("s")
            )],
            with_active.file_binds.clone(),
        ),
        None => (vec![], vec![]),
    };
    let mut counts = FacetCounts::default();
    for (pool, _) in targets {
        add_grouped_counts(&mut counts.kinds, pool, &by_kind, "m.kind", &[], &[]).await?;
        add_grouped_counts(
            &mut counts.extensions,
            pool,
            &by_extension,
            "m.extension",
            &extra,
            &extra_binds,
        )
        .await?;
        if let Some(with_active) = &active {
            counts.extension_base += count_assets(pool, with_active).await?.items;
        }
    }
    for n in counts.extensions.values_mut() {
        *n += counts.extension_base;
    }
    Ok(counts)
}

/// For each of `parent_paths` (`""` for the root) in library `library_id`, the grid's total for
/// `query` in each of its child folders, by child path. A child with no match is left out. Counted
/// once per query, like the total.
#[tauri::command]
pub async fn get_folder_counts(
    query: SearchQuery,
    library_id: String,
    parent_paths: Vec<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<HashMap<String, HashMap<String, i64>>> {
    let library = state
        .db
        .get_library_handle(&library_id)
        .await
        .ok_or_else(|| format!("Library {library_id} is not open"))?;
    folder_counts(&library.pool, &query, &parent_paths)
        .await
        .map_err(|e| e.to_string())
}

/// `get_folder_counts` on the library at `pool`. A folder's count leaves out the query's folder
/// and collection, which selecting the folder replaces (`LibrariesSection`), and groups the files
/// below each parent by the path segment that follows it: one query per parent. Files directly in
/// a parent belong to no child.
async fn folder_counts(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    query: &SearchQuery,
    parent_paths: &[String],
) -> sqlx::Result<HashMap<String, HashMap<String, i64>>> {
    let mut any_folder = query.clone();
    any_folder.folder = None;
    any_folder.collection_id = None;
    let filter = search_filter(&any_folder, false);
    let mut counts = HashMap::new();
    for parent in parent_paths {
        // The files below the parent start with its range's lower bound, `p/`, or are the whole
        // library. SQLite's `substr` and `instr` count characters.
        let range = folder_range(parent);
        let prefix_len = range.as_ref().map_or(0, |[from, _]| from.chars().count());
        let rest = format!("substr(m.file_path, {})", prefix_len + 1);
        let child = format!("substr(m.file_path, 1, {prefix_len} + instr({rest}, '/') - 1)");
        let mut extra = vec![format!("instr({rest}, '/') > 0")];
        if range.is_some() {
            extra.push("m.file_path >= ? AND m.file_path < ?".to_string());
        }
        let binds: Vec<String> = range.into_iter().flatten().collect();
        let mut children = HashMap::new();
        add_grouped_counts(&mut children, pool, &filter, &child, &extra, &binds).await?;
        counts.insert(parent.clone(), children);
    }
    Ok(counts)
}

/// The columns of a `RawAssetRow` selected from `assets a`. A main item with no thumbnail
/// of its own, such as a `.blend`, shows a member's (`structure::preview_stand_ins`). An item's
/// render is shown by its thumbnail when it has one, else by itself (plan C12): as
/// `bundled_preview` when the item is `pictured`, else as `fallback_render`, which cards show only
/// after the item's own thumbnail (plan small-bundled-previews C5).
static ASSET_COLUMNS: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let picture = "(SELECT COALESCE(p.thumbnail_path, p.file_path) FROM assets p
         WHERE p.file_path = a.bundled_preview)";
    let pictured = pictured("a");
    format!(
        r#"
    a.id, a.file_path, a.file_name, a.extension, a.kind,
    a.size_bytes, a.modified_at, a.indexed_at, a.rating,
    a.color_label, a.is_favorite, a.description,
    COALESCE(a.thumbnail_path, CASE WHEN a.group_id = a.id THEN
        (SELECT m.thumbnail_path FROM assets m
         WHERE m.group_id = a.id AND m.thumbnail_path IS NOT NULL
         ORDER BY m.file_path LIMIT 1)
    END) AS thumbnail_path,
    a.preview_status, a.metadata,
    (SELECT json_group_array(tag_id) FROM asset_tags WHERE asset_id = a.id) AS tags,
    (SELECT json_group_array(collection_id) FROM collection_assets WHERE asset_id = a.id) AS collections,
    CASE WHEN a.group_id = a.id THEN
        (SELECT json_group_array(m.extension) FROM assets m WHERE m.group_id = a.id)
    END AS group_formats,
    CASE WHEN {pictured} THEN {picture} END AS bundled_preview,
    CASE WHEN NOT {pictured} THEN {picture} END AS fallback_render"#
    )
});

#[derive(sqlx::FromRow)]
struct RowidAssetRow {
    rowid: i64,
    #[sqlx(flatten)]
    row: RawAssetRow,
}

/// The assets with `rowids`, each with its rowid, in no particular order.
async fn fetch_assets_by_rowid(
    tx: &mut LibraryTx,
    root: &Option<std::path::PathBuf>,
    rowids: &[i64],
) -> sqlx::Result<Vec<(i64, Asset)>> {
    let sql = format!(
        "SELECT a.rowid AS rowid, {columns} FROM assets a WHERE a.rowid IN ({})",
        placeholders(rowids.len()),
        columns = *ASSET_COLUMNS,
    );
    let mut query = sqlx::query_as::<_, RowidAssetRow>(&sql);
    for rowid in rowids {
        query = query.bind(rowid);
    }
    let rows = query.fetch_all(&mut **tx).await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.rowid, row_to_asset(root.as_deref(), r.row)))
        .collect())
}

/// The first `limit` assets matching `filter` in `order_by` order, after skipping `offset`.
async fn fetch_assets(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    root: &Option<std::path::PathBuf>,
    filter: &SqlFilter,
    order_by: &str,
    limit: i64,
    offset: Option<i64>,
) -> sqlx::Result<Vec<Asset>> {
    let sql = format!(
        r#"SELECT {columns}
           FROM assets a
           {}
           ORDER BY {}
           LIMIT ?{}"#,
        filter.where_clause(),
        order_by,
        if offset.is_some() { " OFFSET ?" } else { "" },
        columns = *ASSET_COLUMNS,
    );
    let mut query = sqlx::query_as::<_, RawAssetRow>(&sql);
    for b in filter.binds() {
        query = query.bind(b.clone());
    }
    query = query.bind(limit);
    if let Some(offset) = offset {
        query = query.bind(offset);
    }
    let rows = query.fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| row_to_asset(root.as_deref(), r))
        .collect())
}

#[tauri::command]
pub async fn get_asset(id: String, state: State<'_, Arc<AppState>>) -> Result<Asset> {
    let libs = state.db.get_all_libraries().await;

    // Search across open libraries
    for lib in libs {
        let row = sqlx::query_as::<_, RawAssetRow>(&format!(
            "SELECT {columns} FROM assets a WHERE a.id = ?",
            columns = *ASSET_COLUMNS
        ))
        .bind(&id)
        .fetch_optional(&lib.pool)
        .await
        .map_err(|e| e.to_string())?;

        if let Some(r) = row {
            return Ok(row_to_asset(Some(&lib.root_path), r));
        }
    }

    Err(format!("Asset not found: {}", id))
}

/// The files of the group asset `id` belongs to, for the detail panel: the main item first, then
/// the others in the order the group picks its main item by (`structure::format_rank`), ties by
/// path ignoring case. A file on its own is listed alone.
#[tauri::command]
pub async fn get_group_members(
    id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<GroupMember>> {
    let lib = state
        .db
        .library_of_asset(&id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Asset not found: {}", id))?;
    group_members(&lib, &id).await.map_err(|e| e.to_string())
}

#[derive(sqlx::FromRow)]
struct MemberRow {
    is_main: bool,
    #[sqlx(flatten)]
    row: RawAssetRow,
}

async fn group_members(lib: &LibraryHandle, id: &str) -> sqlx::Result<Vec<GroupMember>> {
    let sql = format!(
        "SELECT a.group_id IS a.id AS is_main, {columns} FROM assets a
         WHERE a.id = ?1 OR a.group_id = (SELECT group_id FROM assets WHERE id = ?1)",
        columns = *ASSET_COLUMNS
    );
    let mut rows = sqlx::query_as::<_, MemberRow>(&sql)
        .bind(id)
        .fetch_all(&lib.pool)
        .await?;
    rows.sort_by_cached_key(|m| {
        (
            !m.is_main,
            structure::format_rank(&m.row.extension),
            m.row.file_path.to_ascii_lowercase(),
            m.row.file_path.clone(),
        )
    });
    Ok(rows
        .into_iter()
        .map(|m| GroupMember {
            folder: m
                .row
                .file_path
                .rsplit_once('/')
                .map_or("", |(dir, _)| dir)
                .to_string(),
            asset: row_to_asset(Some(&lib.root_path), m.row),
        })
        .collect())
}

#[tauri::command]
pub async fn update_asset(
    id: String,
    patch: AssetPatch,
    state: State<'_, Arc<AppState>>,
) -> Result<Asset> {
    apply_asset_patch(&state.db, &id, &patch).await?;
    get_asset(id, state).await
}

/// Writes the patch to the asset's own library only, memberships included. Its metadata (rating,
/// color label, favorite, description and tags) goes to every member of the asset's group
/// (`structure::group_members`), and its thumbnail and preview status to the asset alone.
async fn apply_asset_patch(db: &Database, id: &str, patch: &AssetPatch) -> Result<()> {
    let lib = db
        .library_of_asset(id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Asset not found: {}", id))?;
    // One transaction, so a patch whose tags fail saves none of its other fields either.
    let mut tx = lib.pool.begin().await.map_err(|e| e.to_string())?;
    let members = structure::group_members(&mut *tx, id)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(rating) = patch.rating {
        for member in &members {
            sqlx::query("UPDATE assets SET rating = ? WHERE id = ?")
                .bind(rating)
                .bind(member)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    if let Some(ref color_label) = patch.color_label {
        for member in &members {
            sqlx::query("UPDATE assets SET color_label = ? WHERE id = ?")
                .bind(color_label)
                .bind(member)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    if let Some(is_fav) = patch.is_favorite {
        let fav_int: i64 = if is_fav { 1 } else { 0 };
        for member in &members {
            sqlx::query("UPDATE assets SET is_favorite = ? WHERE id = ?")
                .bind(fav_int)
                .bind(member)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    if let Some(ref description) = patch.description {
        for member in &members {
            sqlx::query("UPDATE assets SET description = ? WHERE id = ?")
                .bind(description)
                .bind(member)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    if let Some(ref thumbnail_path) = patch.thumbnail_path {
        let rel_thumb = make_relative_path(&lib.root_path, Path::new(thumbnail_path));
        sqlx::query("UPDATE assets SET thumbnail_path = ? WHERE id = ?")
            .bind(rel_thumb)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }
    if let Some(ref preview_status) = patch.preview_status {
        sqlx::query("UPDATE assets SET preview_status = ? WHERE id = ?")
            .bind(preview_status)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }
    if let Some(ref tags) = patch.tags {
        for member in &members {
            sqlx::query("DELETE FROM asset_tags WHERE asset_id = ?")
                .bind(member)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
        for tag_id in tags {
            // Copy master's definition, and its ancestors root first, into the mirror: a
            // mirror can lack them after a skipped name collision or a failed mirror write, and
            // a foreign-key-enforcing connection would then reject the membership or the tag.
            let chain = db
                .tag_with_ancestors(tag_id)
                .await
                .map_err(|e| e.to_string())?;
            let Some(tag) = chain.last() else {
                tracing::warn!(
                    "Tag {} is not in the catalog, not tagging {} with it",
                    tag_id,
                    id
                );
                continue;
            };
            for definition in &chain {
                upsert_tag(&mut *tx, definition)
                    .await
                    .map_err(|e| format!("Could not tag the asset with '{}': {}", tag.1, e))?;
            }
            for member in &members {
                sqlx::query("INSERT OR IGNORE INTO asset_tags (asset_id, tag_id) VALUES (?, ?)")
                    .bind(member)
                    .bind(tag_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
    }

    tx.commit().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn delete_asset(
    id: String,
    delete_file: Option<bool>,
    state: State<'_, Arc<AppState>>,
) -> Result<()> {
    let result = delete_assets_with(
        &state.db,
        std::slice::from_ref(&id),
        delete_file.unwrap_or(true),
        |path| trash::delete(path),
    )
    .await;
    // Emit even on failure: the files of a group that were trashed are gone.
    let _ = state.app_handle.emit("library:updated", ());
    result.map_err(|e| e.message)
}

/// One file that a delete or rename of an item reaches (`item_files`).
#[derive(sqlx::FromRow)]
struct ItemFile {
    id: String,
    file_path: String,
    file_name: String,
    extension: String,
    /// Whether this is the item's bundled preview rather than a member
    is_render: bool,
}

/// The files a delete or rename of `id` reaches (plan C21): every member of its group, the main
/// item first and the rest by path, then the group's bundled preview, unless another item shows
/// the same render. A file on its own comes with its bundled preview, if it has one. Empty when
/// `pool` holds no asset `id`.
async fn item_files(pool: &sqlx::Pool<sqlx::Sqlite>, id: &str) -> sqlx::Result<Vec<ItemFile>> {
    let mut files: Vec<ItemFile> = sqlx::query_as(
        "WITH item AS (SELECT COALESCE(group_id, id) AS main FROM assets WHERE id = ?1)
         SELECT a.id, a.file_path, a.file_name, a.extension, 0 AS is_render FROM assets a, item
         WHERE a.id = ?1 OR a.group_id = item.main
         ORDER BY a.id <> item.main, a.file_path",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let render: Option<ItemFile> = sqlx::query_as(
        "SELECT p.id, p.file_path, p.file_name, p.extension, 1 AS is_render FROM assets m
         JOIN assets p ON p.file_path = m.bundled_preview
         WHERE m.id = (SELECT COALESCE(group_id, id) FROM assets WHERE id = ?)
           AND NOT EXISTS (SELECT 1 FROM assets o
                           WHERE o.bundled_preview = m.bundled_preview AND o.id <> m.id)",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    files.extend(render);
    Ok(files)
}

/// Deletes the rows of `id`'s files (`item_files`) from `lib` and, when `delete_file` is set,
/// first moves each file to the trash with `trash`, never falling back to a permanent delete. A
/// failed trash stops there: the files trashed before it lose their rows, and the error names
/// the files left in place, which keep theirs.
async fn delete_asset_with<E: std::fmt::Display>(
    db: &Database,
    lib: &LibraryHandle,
    id: &str,
    delete_file: bool,
    trash: impl Fn(&Path) -> std::result::Result<(), E>,
) -> Result<()> {
    let files = item_files(&lib.pool, id).await.map_err(|e| e.to_string())?;
    let mut failure = None;
    let mut trashed = 0;
    for file in &files {
        let path = PathBuf::from(resolve_path(&lib.root_path, &file.file_path));
        if delete_file && path.exists() {
            if let Err(e) = trash(&path) {
                failure = Some(format!(
                    "Could not move {} to the trash, so it was not deleted: {}",
                    path.display(),
                    e
                ));
                break;
            }
        }
        trashed += 1;
    }

    let gone: Vec<&str> = files[..trashed].iter().map(|f| f.id.as_str()).collect();
    if let Err(e) = delete_rows(&lib.pool, &gone).await {
        tracing::warn!(
            "Could not delete the rows of {gone:?} from library {}: {e}",
            lib.id
        );
    }
    let _ = delete_rows(&db.master_pool, &gone).await;
    let count: sqlx::Result<i64> = sqlx::query_scalar("SELECT COUNT(*) FROM assets")
        .fetch_one(&lib.pool)
        .await;
    if let Ok(count) = count {
        let _ = sqlx::query("UPDATE libraries SET asset_count = ? WHERE id = ?")
            .bind(count)
            .bind(&lib.id)
            .execute(&db.master_pool)
            .await;
    }

    let Some(mut message) = failure else {
        return Ok(());
    };
    let left = &files[trashed..];
    if left.len() > 1 {
        let paths: Vec<&str> = left.iter().map(|f| f.file_path.as_str()).collect();
        message.push_str(&format!(". Still in place: {}", paths.join(", ")));
    }
    Err(message)
}

/// Deletes the assets `ids` and their tag, collection and preview job rows from `pool`, in one
/// transaction.
async fn delete_rows(pool: &sqlx::Pool<sqlx::Sqlite>, ids: &[&str]) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    for id in ids {
        for sql in [
            "DELETE FROM asset_tags WHERE asset_id = ?",
            "DELETE FROM collection_assets WHERE asset_id = ?",
            "DELETE FROM preview_jobs WHERE asset_id = ?",
            "DELETE FROM assets WHERE id = ?",
        ] {
            sqlx::query(sql).bind(id).execute(&mut *tx).await?;
        }
    }
    tx.commit().await
}

#[tauri::command]
pub async fn delete_assets(
    ids: Vec<String>,
    delete_file: Option<bool>,
    state: State<'_, Arc<AppState>>,
) -> std::result::Result<(), DeleteAssetsError> {
    let result = delete_assets_with(&state.db, &ids, delete_file.unwrap_or(true), |path| {
        trash::delete(path)
    })
    .await;
    // Emit even on failure: the assets that were trashed are gone.
    let _ = state.app_handle.emit("library:updated", ());
    result
}

/// A multi-delete where some assets were not deleted. `message` is for the user: the error
/// itself for one failure, a count and the first error for several. `failed_ids` lets the UI
/// drop the assets that were deleted and keep the rest.
#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteAssetsError {
    message: String,
    failed_ids: Vec<String>,
}

/// Deletes each asset with its group and bundled preview (`delete_asset_with`), carrying on past
/// failures, and reports them. Before it returns, it reruns the structure pass once for each
/// library that held one of them, failed or not, since a group can lose some of its files.
async fn delete_assets_with<E: std::fmt::Display>(
    db: &Database,
    ids: &[String],
    delete_file: bool,
    trash: impl Fn(&Path) -> std::result::Result<(), E>,
) -> std::result::Result<(), DeleteAssetsError> {
    let mut failures = Vec::new();
    let mut changed: Vec<LibraryHandle> = Vec::new();
    for id in ids {
        let lib = match db.library_of_asset(id).await {
            Ok(Some(lib)) => lib,
            Ok(None) => {
                // A row only master holds, from before libraries had their own DBs.
                let _ = delete_rows(&db.master_pool, &[id.as_str()]).await;
                continue;
            }
            Err(e) => {
                failures.push((id.clone(), e.to_string()));
                continue;
            }
        };
        if let Err(e) = delete_asset_with(db, &lib, id, delete_file, &trash).await {
            failures.push((id.clone(), e));
        }
        if !changed.iter().any(|l| l.id == lib.id) {
            changed.push(lib);
        }
    }
    for lib in &changed {
        structure::rebuild_library(db, lib).await;
    }
    let message = match failures.len() {
        0 => return Ok(()),
        1 => failures[0].1.clone(),
        n => format!("{} files were not deleted. {}", n, failures[0].1),
    };
    Err(DeleteAssetsError {
        message,
        failed_ids: failures.into_iter().map(|(id, _)| id).collect(),
    })
}

#[tauri::command]
pub async fn rename_asset(
    id: String,
    new_name: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Asset> {
    rename_asset_with(&state.db, &id, &new_name, |from, to| {
        std::fs::rename(from, to)
    })
    .await?;
    let _ = state.app_handle.emit("library:updated", ());
    get_asset(id, state).await
}

/// One file's rename: its row, where it is and where it goes.
struct Move<'a> {
    file: &'a ItemFile,
    from: PathBuf,
    to: PathBuf,
    name: String,
    extension: String,
}

/// Renames `id`'s files (`item_files`) in their folders, with `rename`, and their rows, then
/// reruns the library's structure pass. A file on its own takes `new_name`, with its extension
/// added when the name has none, so a typed extension changes its format. A group's members take
/// `new_name` as their stem, less the main item's extension if it ends with it, and each keeps
/// its own extension (plan C21). Either way the bundled preview takes the new stem and keeps its
/// extension. Nothing moves when a new name is taken, and a failed rename or row write
/// puts the files already renamed back.
async fn rename_asset_with(
    db: &Database,
    id: &str,
    new_name: &str,
    rename: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<()> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return Err("Asset file name cannot be empty, '.' or '..'".into());
    }

    if trimmed.contains(['/', '\0']) {
        return Err("File name cannot contain '/' or NUL".into());
    }

    let lib = db
        .library_of_asset(id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Asset not found: {}", id))?;
    let files = item_files(&lib.pool, id).await.map_err(|e| e.to_string())?;
    let Some(main) = files.first() else {
        return Err(format!("Asset not found: {}", id));
    };

    // Each file's new name and extension
    let with_stem = |f: &ItemFile, stem: &str| match f.file_name.rsplit_once('.') {
        Some((_, ext)) => (format!("{stem}.{ext}"), f.extension.clone()),
        None => (stem.to_string(), f.extension.clone()),
    };
    let names: Vec<(String, String)> = if files.iter().filter(|f| !f.is_render).count() == 1 {
        let old_ext = &main.extension;
        let (name, ext) = match trimmed.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
                (trimmed.to_string(), ext.to_lowercase())
            }
            _ => (format!("{}.{}", trimmed, old_ext), old_ext.clone()),
        };
        let stem = name
            .rsplit_once('.')
            .map_or(name.as_str(), |(stem, _)| stem);
        files
            .iter()
            .map(|f| {
                if f.is_render {
                    with_stem(f, stem)
                } else {
                    (name.clone(), ext.clone())
                }
            })
            .collect()
    } else {
        let stem = match trimmed.rsplit_once('.') {
            Some((stem, ext)) if ext.eq_ignore_ascii_case(&main.extension) => stem,
            _ => trimmed,
        };
        if stem.is_empty() {
            return Err("Asset file name cannot be empty, '.' or '..'".into());
        }
        files.iter().map(|f| with_stem(f, stem)).collect()
    };
    let moves: Vec<Move> = files
        .iter()
        .zip(names)
        .map(|(file, (name, extension))| {
            let from = PathBuf::from(resolve_path(&lib.root_path, &file.file_path));
            Move {
                file,
                to: from.with_file_name(&name),
                from,
                name,
                extension,
            }
        })
        .collect();

    // Refuse before anything moves if a new name is taken, or two files would take one name
    let mut targets = HashSet::new();
    let taken: Vec<&Move> = moves
        .iter()
        .filter(|m| {
            let repeated = !targets.insert(&m.to);
            repeated || (m.to != m.from && m.to.exists())
        })
        .collect();
    match taken.as_slice() {
        [] => {}
        [m] if files.len() == 1 => {
            return Err(format!(
                "A file named '{}' already exists in this folder",
                m.name
            ));
        }
        taken => {
            let paths: Vec<String> = taken
                .iter()
                .map(|m| make_relative_path(&lib.root_path, &m.to))
                .collect();
            return Err(format!(
                "Nothing was renamed, because these names are taken: {}",
                paths.join(", ")
            ));
        }
    }

    let mut done: Vec<&Move> = Vec::new();
    for m in &moves {
        if m.to == m.from || !m.from.exists() {
            continue;
        }
        if let Err(e) = rename(&m.from, &m.to) {
            return Err(match undo_renames(&done, &rename) {
                Ok(()) => format!("Failed to rename file on disk: {e}"),
                Err(left) => format!(
                    "Failed to rename file on disk ({e}), and could not undo the renames of: {left}"
                ),
            });
        }
        done.push(m);
    }

    // The library's rows must follow the files: if they can't (a busy NAS), the files take their
    // old names back, so the index never sees new files.
    let now = chrono::Utc::now().timestamp_millis();
    let rows: Vec<(&str, String, &str, &str)> = moves
        .iter()
        .map(|m| {
            let path = make_relative_path(&lib.root_path, &m.to);
            (
                m.name.as_str(),
                path,
                m.extension.as_str(),
                m.file.id.as_str(),
            )
        })
        .collect();
    let write = |pool: sqlx::Pool<sqlx::Sqlite>| {
        let rows = &rows;
        async move {
            let mut tx = pool.begin().await?;
            for (name, path, ext, id) in rows {
                sqlx::query(
                    "UPDATE assets SET file_name = ?, file_path = ?, extension = ?, modified_at = ? WHERE id = ?",
                )
                .bind(name)
                .bind(path)
                .bind(ext)
                .bind(now)
                .bind(id)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await
        }
    };
    if let Err(e) = write(lib.pool.clone()).await {
        return Err(match undo_renames(&done, &rename) {
            Ok(()) => format!("Could not record the rename, so it was undone: {e}"),
            Err(left) => {
                format!("Could not record the rename ({e}), and could not undo it on disk: {left}")
            }
        });
    }
    let _ = write(db.master_pool.clone()).await;

    structure::rebuild_library(db, &lib).await;
    Ok(())
}

/// Puts the files of `done` back at their old names, last first. On failure, names the files
/// left at their new names.
fn undo_renames(
    done: &[&Move],
    rename: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> std::result::Result<(), String> {
    let left: Vec<String> = done
        .iter()
        .rev()
        .filter_map(|m| {
            rename(&m.to, &m.from)
                .err()
                .map(|e| format!("{} ({e})", m.to.display()))
        })
        .collect();
    if left.is_empty() {
        Ok(())
    } else {
        Err(left.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        index_file_paths, indexed_rowids, insert_asset, insert_collection, insert_tag,
        memory_database, memory_library, memory_library_pool,
        memory_library_pool_enforcing_foreign_keys, memory_library_pool_with_old_fts,
        LibraryHandle,
    };
    use sqlx::{Pool, Row, Sqlite};
    use std::cell::RefCell;
    use std::path::PathBuf;

    #[test]
    fn fts_match_expression_quotes_each_term_as_a_prefix_string() {
        assert_eq!(
            fts_match_expression("low-poly tree.glb").as_deref(),
            Some(r#""low-poly"* "tree.glb"*"#)
        );
        assert_eq!(fts_match_expression("AND").as_deref(), Some(r#""AND"*"#));
        assert_eq!(
            fts_match_expression(r#"say"hi"#).as_deref(),
            Some(r#""say""hi"*"#)
        );
    }

    #[test]
    fn fts_match_expression_drops_terms_without_letters_or_digits() {
        assert_eq!(fts_match_expression(""), None);
        assert_eq!(fts_match_expression("  \" ( "), None);
        assert_eq!(
            fts_match_expression("( tree").as_deref(),
            Some(r#""tree"*"#)
        );
    }

    async fn search_names(pool: &sqlx::Pool<sqlx::Sqlite>, text: &str) -> Vec<String> {
        let payload: SearchPayload = serde_json::from_value(serde_json::json!({
            "query": {
                "text": text,
                "filter": {},
                "sort": { "field": "name", "order": "asc" },
            },
            "page": 0,
            "pageSize": 100,
        }))
        .unwrap();
        let result = search_libraries(&payload, &[(pool.clone(), None)])
            .await
            .unwrap_or_else(|e| panic!("search for {text:?} failed: {e}"));
        result.assets.into_iter().map(|a| a.file_name).collect()
    }

    #[tokio::test]
    async fn search_matches_ordinary_text_that_is_fts5_syntax() {
        let pool = memory_library_pool().await;
        for name in [
            "brick.png",
            "low-poly-rock.fbx",
            "rock_and_moss.png",
            "tree.glb",
        ] {
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
                 VALUES (?, ?, ?, '', 'texture', 0, 0, 0)",
            )
            .bind(name)
            .bind(name)
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
        }
        let all = [
            "brick.png",
            "low-poly-rock.fbx",
            "rock_and_moss.png",
            "tree.glb",
        ];

        assert_eq!(search_names(&pool, "low-poly").await, ["low-poly-rock.fbx"]);
        assert_eq!(search_names(&pool, "tree.glb").await, ["tree.glb"]);
        assert_eq!(search_names(&pool, "AND").await, ["rock_and_moss.png"]);
        assert_eq!(search_names(&pool, "\"").await, all);
        assert_eq!(search_names(&pool, "(").await, all);
        assert_eq!(search_names(&pool, "( tree").await, ["tree.glb"]);
    }

    /// Inserts an asset at `path` (relative to the library root), named for its file name.
    async fn insert_at(pool: &Pool<Sqlite>, path: &str, description: &str) {
        let name = path.rsplit('/').next().unwrap();
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, description)
             VALUES (?, ?, ?, '', 'texture', 0, 0, 0, ?)",
        )
        .bind(name)
        .bind(path)
        .bind(name)
        .bind(description)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Two platformer packs, one other pack and a file at the root.
    async fn insert_packs(pool: &Pool<Sqlite>) {
        for (path, description) in [
            ("3D assets/Platformer Kit/Models/crate.glb", ""),
            ("2D assets/Pixel Platformer/Tiles/tile.png", ""),
            ("2D assets/Space Shooter/ship.png", "hull"),
            ("platformer-notes.txt", ""),
        ] {
            insert_at(pool, path, description).await;
        }
    }

    #[tokio::test]
    async fn search_finds_an_asset_by_the_folder_names_in_its_path() {
        let pool = memory_library_pool().await;
        insert_packs(&pool).await;

        assert_eq!(
            search_names(&pool, "platformer").await,
            ["crate.glb", "platformer-notes.txt", "tile.png"]
        );
        assert_eq!(search_names(&pool, "3d models").await, ["crate.glb"]);
        assert_eq!(search_names(&pool, "ship").await, ["ship.png"]);
        assert_eq!(search_names(&pool, "hull").await, ["ship.png"]);
    }

    #[tokio::test]
    async fn search_finds_assets_by_folder_names_once_an_old_library_is_converted() {
        let pool = memory_library_pool_with_old_fts().await;
        insert_packs(&pool).await;
        assert_eq!(
            search_names(&pool, "platformer").await,
            ["platformer-notes.txt"]
        );

        assert!(index_file_paths(&pool, false).await.unwrap());

        assert_eq!(
            search_names(&pool, "platformer").await,
            ["crate.glb", "platformer-notes.txt", "tile.png"]
        );
        assert_eq!(search_names(&pool, "ship").await, ["ship.png"]);
        assert_eq!(search_names(&pool, "hull").await, ["ship.png"]);
    }

    #[tokio::test]
    async fn after_a_rename_and_a_delete_the_index_matches_only_the_new_path() {
        let pool = memory_library_pool().await;
        insert_at(&pool, "Platformer Kit/crate.glb", "").await;
        insert_at(&pool, "Space Kit/ship.png", "").await;

        // What `rename_asset` writes.
        sqlx::query(
            "UPDATE assets SET file_name = 'barrel.glb', file_path = 'Platformer Kit/barrel.glb',
                 extension = 'glb', modified_at = 1
             WHERE id = 'crate.glb'",
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(indexed_rowids(&pool, "crate").await, Vec::<i64>::new());
        assert_eq!(search_names(&pool, "barrel").await, ["barrel.glb"]);
        assert_eq!(search_names(&pool, "platformer").await, ["barrel.glb"]);

        // A move to another folder changes only the path.
        sqlx::query("UPDATE assets SET file_path = 'Space Kit/barrel.glb' WHERE id = 'crate.glb'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(indexed_rowids(&pool, "platformer").await, Vec::<i64>::new());
        assert_eq!(
            search_names(&pool, "space").await,
            ["barrel.glb", "ship.png"]
        );

        sqlx::query("DELETE FROM assets WHERE id = 'crate.glb'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(indexed_rowids(&pool, "barrel").await, Vec::<i64>::new());
        assert_eq!(indexed_rowids(&pool, "space").await, [2]);
        assert_eq!(search_names(&pool, "space").await, ["ship.png"]);
    }

    /// The acceptance check on a real library made before folder search, such as a copy of the
    /// golden snapshot's DB (docs/agents/running-the-app.md#bench-data). Opening the copy
    /// converts it, and every file in a folder whose name has a word starting "platformer" must
    /// then match the search "platformer". Run it with
    /// `OPENDAM_FTS_LIBRARY=<copy> cargo nextest run --run-ignored only real_library`.
    #[tokio::test]
    #[ignore = "needs OPENDAM_FTS_LIBRARY, a copy of a library DB it may convert"]
    async fn a_converted_real_library_finds_every_platformer_pack_by_its_folder_name() {
        let file = PathBuf::from(std::env::var("OPENDAM_FTS_LIBRARY").unwrap());
        let started = std::time::Instant::now();
        let pool = crate::db::connect_library_sqlite(&file).await.unwrap();
        println!("opened and converted in {:?}", started.elapsed());

        let in_platformer_folder = |path: &str| {
            path.rsplit_once('/').is_some_and(|(folders, _)| {
                folders
                    .to_lowercase()
                    .split(|c: char| !c.is_alphanumeric())
                    .any(|word| word.starts_with("platformer"))
            })
        };
        let pack = |path: &str| path.splitn(3, '/').take(2).collect::<Vec<_>>().join("/");
        let paths: Vec<String> = sqlx::query_scalar("SELECT file_path FROM assets")
            .fetch_all(&pool)
            .await
            .unwrap();
        let expected: std::collections::BTreeSet<&String> =
            paths.iter().filter(|p| in_platformer_folder(p)).collect();

        let payload: SearchPayload = serde_json::from_value(serde_json::json!({
            "query": { "text": "platformer", "filter": {}, "sort": { "field": "name", "order": "asc" } },
            "page": 0,
            "pageSize": paths.len(),
        }))
        .unwrap();
        let result = search_libraries(&payload, &[(pool, None)]).await.unwrap();
        let found: std::collections::BTreeSet<String> =
            result.assets.into_iter().map(|a| a.file_path).collect();

        let mut packs = std::collections::BTreeMap::<String, usize>::new();
        for path in &expected {
            *packs.entry(pack(path)).or_default() += 1;
        }
        for (pack, files) in &packs {
            println!("{files:>5} files in a platformer folder of {pack}");
        }
        let missing: Vec<_> = expected.iter().filter(|p| !found.contains(**p)).collect();
        println!(
            "{} packs, {} files in platformer folders, {} matched the search, {} missing",
            packs.len(),
            expected.len(),
            found.len(),
            missing.len()
        );
        assert!(!packs.is_empty());
        assert!(missing.is_empty(), "not found: {missing:?}");
    }

    /// A library with this collection tree, each asset (named for its id) in the collections
    /// listed after it:
    ///
    /// ```text
    /// props            crate
    ///   furniture      sofa, log
    ///     chairs       stool
    /// nature           rock
    ///   trees          oak, log
    /// empty
    /// (none)           loose
    /// ```
    async fn collection_library() -> Pool<Sqlite> {
        let pool = memory_library_pool().await;
        for (id, parent) in [
            ("props", None),
            ("furniture", Some("props")),
            ("chairs", Some("furniture")),
            ("nature", None),
            ("trees", Some("nature")),
            ("empty", None),
        ] {
            insert_collection(&pool, id, id, parent).await;
        }
        for (asset, collections) in [
            ("crate", &["props"][..]),
            ("sofa", &["furniture"]),
            ("stool", &["chairs"]),
            ("rock", &["nature"]),
            ("oak", &["trees"]),
            ("log", &["furniture", "trees"]),
            ("loose", &[]),
        ] {
            insert_asset(&pool, asset).await;
            for collection in collections {
                sqlx::query(
                    "INSERT INTO collection_assets (collection_id, asset_id) VALUES (?, ?)",
                )
                .bind(collection)
                .bind(asset)
                .execute(&pool)
                .await
                .unwrap();
            }
        }
        pool
    }

    /// The ids of the assets in `pools` matching `query` (a JSON `SearchQuery` without its
    /// sort, which is by name), in name order.
    async fn search_ids(pools: &[Pool<Sqlite>], query: serde_json::Value) -> Vec<String> {
        search_page(pools, query, 0, 100).await.0
    }

    /// Page `page` of `page_size` assets in `pools` matching `query` (as for `search_ids`): their
    /// ids in name order, and the total number of matches.
    async fn search_page(
        pools: &[Pool<Sqlite>],
        query: serde_json::Value,
        page: i64,
        page_size: i64,
    ) -> (Vec<String>, i64) {
        let mut query = query;
        query["sort"] = serde_json::json!({ "field": "name", "order": "asc" });
        if query.get("text").is_none() {
            query["text"] = "".into();
        }
        let payload: SearchPayload = serde_json::from_value(serde_json::json!({
            "query": query,
            "page": page,
            "pageSize": page_size,
        }))
        .unwrap();
        let libs: Vec<_> = pools.iter().map(|p| (p.clone(), None)).collect();
        let result = search_libraries(&payload, &libs)
            .await
            .unwrap_or_else(|e| panic!("search for {query} failed: {e}"));
        (
            result.assets.into_iter().map(|a| a.id).collect(),
            result
                .total
                .expect("a search counts its matches by default"),
        )
    }

    /// The ids matching a filter on `collectionIds` with `logic`, with or without subcollections.
    async fn in_collections(
        pool: &Pool<Sqlite>,
        ids: &[&str],
        logic: &str,
        include_sub: bool,
    ) -> Vec<String> {
        let filter = serde_json::json!({
            "collectionIds": ids,
            "collectionLogic": logic,
            "includeSubcollections": include_sub,
        });
        search_ids(
            std::slice::from_ref(pool),
            serde_json::json!({ "filter": filter }),
        )
        .await
    }

    #[tokio::test]
    async fn searching_a_collection_includes_its_nested_subcollections_by_default() {
        let pool = collection_library().await;
        let search = |query| search_ids(std::slice::from_ref(&pool), query);

        assert_eq!(
            search(serde_json::json!({ "filter": {}, "collectionId": "props" })).await,
            ["crate", "log", "sofa", "stool"]
        );
        assert_eq!(
            search(serde_json::json!({ "filter": { "collectionId": "props" } })).await,
            ["crate", "log", "sofa", "stool"]
        );
        assert_eq!(
            search(serde_json::json!({ "filter": {}, "collectionId": "empty" })).await,
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn searching_a_collection_without_subcollections_matches_only_its_own_assets() {
        let pool = collection_library().await;
        let search = |query| search_ids(std::slice::from_ref(&pool), query);

        assert_eq!(
            search(serde_json::json!({
                "filter": {},
                "collectionId": "props",
                "includeSubcollections": false,
            }))
            .await,
            ["crate"]
        );
        // The filter's setting wins over the query's.
        assert_eq!(
            search(serde_json::json!({
                "filter": { "includeSubcollections": false },
                "collectionId": "props",
                "includeSubcollections": true,
            }))
            .await,
            ["crate"]
        );
        assert_eq!(
            search(serde_json::json!({
                "filter": { "includeSubcollections": true },
                "collectionId": "props",
                "includeSubcollections": false,
            }))
            .await,
            ["crate", "log", "sofa", "stool"]
        );
    }

    #[tokio::test]
    async fn searching_any_of_several_collections_matches_assets_in_one_of_them() {
        let pool = collection_library().await;

        assert_eq!(
            in_collections(&pool, &["furniture", "nature"], "or", true).await,
            ["log", "oak", "rock", "sofa", "stool"]
        );
        assert_eq!(
            in_collections(&pool, &["furniture", "nature"], "or", false).await,
            ["log", "rock", "sofa"]
        );
        // "or" is the default logic.
        assert_eq!(
            search_ids(
                std::slice::from_ref(&pool),
                serde_json::json!({ "filter": { "collectionIds": ["chairs", "trees"] } })
            )
            .await,
            ["log", "oak", "stool"]
        );
    }

    #[tokio::test]
    async fn searching_all_of_several_collections_matches_assets_in_every_one() {
        let pool = collection_library().await;

        assert_eq!(
            in_collections(&pool, &["props", "nature"], "and", true).await,
            ["log"]
        );
        assert_eq!(
            in_collections(&pool, &["props", "nature"], "and", false).await,
            Vec::<String>::new()
        );
        assert_eq!(
            in_collections(&pool, &["furniture", "trees"], "and", false).await,
            ["log"]
        );
        assert_eq!(
            in_collections(&pool, &["props"], "and", true).await,
            ["crate", "log", "sofa", "stool"]
        );
    }

    #[tokio::test]
    async fn searching_a_primary_collection_and_any_other_matches_assets_in_both() {
        let pool = collection_library().await;

        assert_eq!(
            in_collections(&pool, &["props", "chairs", "nature"], "and_or", true).await,
            ["log", "stool"]
        );
        assert_eq!(
            in_collections(&pool, &["props", "chairs", "nature"], "and_or", false).await,
            Vec::<String>::new()
        );
        assert_eq!(
            in_collections(&pool, &["furniture", "chairs", "trees"], "and_or", false).await,
            ["log"]
        );
        assert_eq!(
            in_collections(&pool, &["furniture"], "and_or", true).await,
            ["log", "sofa", "stool"]
        );
        assert_eq!(
            in_collections(&pool, &["furniture"], "and_or", false).await,
            ["log", "sofa"]
        );
    }

    #[tokio::test]
    async fn a_query_collection_and_filter_collections_combine_without_duplicates() {
        let pool = collection_library().await;
        let search = |query| search_ids(std::slice::from_ref(&pool), query);

        // props is the primary collection once, not also one of the others.
        assert_eq!(
            search(serde_json::json!({
                "filter": { "collectionIds": ["props", "nature"], "collectionLogic": "and_or" },
                "collectionId": "props",
            }))
            .await,
            ["log"]
        );
        assert_eq!(
            search(serde_json::json!({
                "filter": { "collectionIds": ["", "trees"], "collectionLogic": "and" },
                "collectionId": "furniture",
            }))
            .await,
            ["log"]
        );
    }

    #[tokio::test]
    async fn searching_unorganized_assets_matches_assets_in_no_collection() {
        let pool = collection_library().await;
        let search = |query| search_ids(std::slice::from_ref(&pool), query);

        assert_eq!(
            in_collections(&pool, &["props"], "none", true).await,
            ["loose"]
        );
        assert_eq!(
            search(serde_json::json!({
                "filter": { "unorganized": true, "collectionIds": ["props"] },
            }))
            .await,
            ["loose"]
        );
    }

    #[tokio::test]
    async fn a_collection_filter_combines_with_text_tag_and_name_filters() {
        let pool = collection_library().await;
        insert_tag(&pool, "wood", "wood").await;
        for asset in ["log", "oak", "sofa"] {
            sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES (?, 'wood')")
                .bind(asset)
                .execute(&pool)
                .await
                .unwrap();
        }
        let search = |query| search_ids(std::slice::from_ref(&pool), query);

        assert_eq!(
            search(serde_json::json!({
                "text": "sofa",
                "filter": {
                    "tags": ["wood"],
                    "collectionIds": ["props", "furniture"],
                    "collectionLogic": "and_or",
                    "nameSuffix": ".glb",
                },
            }))
            .await,
            ["sofa"]
        );
        assert_eq!(
            search(serde_json::json!({
                "text": "oak",
                "filter": {
                    "tags": ["wood"],
                    "collectionIds": ["nature"],
                    "namePrefix": "o",
                },
            }))
            .await,
            ["oak"]
        );
    }

    #[tokio::test]
    async fn a_collection_filter_applies_to_each_library_searched() {
        let a = collection_library().await;
        let b = collection_library().await;

        let ids = search_ids(
            &[a, b],
            serde_json::json!({ "filter": {}, "collectionId": "furniture" }),
        )
        .await;

        assert_eq!(ids, ["log", "log", "sofa", "sofa", "stool", "stool"]);
    }

    /// A library with assets `a` to `f` (named for their ids) tagged with:
    ///
    /// ```text
    /// a   (none)
    /// b   stone
    /// c   stone, moss
    /// d   moss, wet
    /// e   stone, moss, wet
    /// f   wet
    /// ```
    async fn tag_library() -> Pool<Sqlite> {
        let pool = memory_library_pool().await;
        for tag in ["stone", "moss", "wet"] {
            insert_tag(&pool, tag, tag).await;
        }
        for (asset, tags) in [
            ("a", &[][..]),
            ("b", &["stone"]),
            ("c", &["stone", "moss"]),
            ("d", &["moss", "wet"]),
            ("e", &["stone", "moss", "wet"]),
            ("f", &["wet"]),
        ] {
            insert_asset(&pool, asset).await;
            for tag in tags {
                sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES (?, ?)")
                    .bind(asset)
                    .bind(tag)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
        pool
    }

    #[tokio::test]
    async fn searching_by_tag_follows_the_tag_logic() {
        let pool = tag_library().await;
        let tagged_with = |tags: &[&str], logic: Option<&str>| {
            let mut filter = serde_json::json!({ "tags": tags });
            if let Some(logic) = logic {
                filter["tagLogic"] = logic.into();
            }
            search_ids(
                std::slice::from_ref(&pool),
                serde_json::json!({ "filter": filter }),
            )
        };

        // "or" (the default): any of the tags.
        assert_eq!(tagged_with(&["wet"], None).await, ["d", "e", "f"]);
        assert_eq!(
            tagged_with(&["stone", "wet"], Some("or")).await,
            ["b", "c", "d", "e", "f"]
        );
        // "and" should match assets with every tag (c and e for stone and moss), but the tag
        // count is bound as text and SQLite never finds COUNT(...) equal to text, so it
        // matches nothing. This pins today's behaviour; fixing it is a separate change.
        let none = Vec::<String>::new();
        assert_eq!(tagged_with(&["stone", "moss"], Some("and")).await, none);
        assert_eq!(tagged_with(&["moss", "wet"], Some("and")).await, none);
        assert_eq!(
            tagged_with(&["stone", "moss", "wet"], Some("and")).await,
            none
        );
        // "and_or": the first tag and any of the rest.
        assert_eq!(
            tagged_with(&["moss"], Some("and_or")).await,
            ["c", "d", "e"]
        );
        assert_eq!(
            tagged_with(&["stone", "moss", "wet"], Some("and_or")).await,
            ["c", "e"]
        );
        assert_eq!(
            tagged_with(&["wet", "stone", "moss"], Some("and_or")).await,
            ["d", "e"]
        );
        // "none": untagged, whatever tags are given.
        assert_eq!(tagged_with(&[], Some("none")).await, ["a"]);
        assert_eq!(tagged_with(&["stone"], Some("none")).await, ["a"]);
        // No tags with any other logic filters nothing.
        assert_eq!(
            tagged_with(&[], Some("and")).await,
            ["a", "b", "c", "d", "e", "f"]
        );
    }

    /// A library with these assets, named `<id>.<extension>` and stored under `<kind>s/`:
    ///
    /// ```text
    /// id  kind     ext  color  fav  preview     rating  size  modified
    /// m1  model    glb  red    0    error       1       300   30
    /// m2  model    fbx  blue   1    done        0       400   40
    /// m3  model    glb  none   0    processing  2       500   50
    /// p1  texture  png  red    1    done        5       100   10
    /// p2  texture  png  none   0    none        3       200   20
    /// ```
    async fn attribute_library() -> Pool<Sqlite> {
        let pool = memory_library_pool().await;
        for (id, kind, ext, color, fav, preview, rating, size, modified) in [
            ("m1", "model", "glb", "red", 0, "error", 1, 300, 30),
            ("m2", "model", "fbx", "blue", 1, "done", 0, 400, 40),
            ("m3", "model", "glb", "none", 0, "processing", 2, 500, 50),
            ("p1", "texture", "png", "red", 1, "done", 5, 100, 10),
            ("p2", "texture", "png", "none", 0, "none", 3, 200, 20),
        ] {
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, color_label, is_favorite,
                                     preview_status, rating, size_bytes, modified_at, indexed_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
            )
            .bind(id)
            .bind(format!("{kind}s/{id}.{ext}"))
            .bind(format!("{id}.{ext}"))
            .bind(ext)
            .bind(kind)
            .bind(color)
            .bind(fav)
            .bind(preview)
            .bind(rating)
            .bind(size)
            .bind(modified)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    #[tokio::test]
    async fn searching_by_asset_attributes_matches_each_filter() {
        let pool = attribute_library().await;
        let search = |filter| {
            search_ids(
                std::slice::from_ref(&pool),
                serde_json::json!({ "filter": filter }),
            )
        };
        use serde_json::json;

        assert_eq!(
            search(json!({ "kinds": ["model"] })).await,
            ["m1", "m2", "m3"]
        );
        assert_eq!(
            search(json!({ "colorLabels": ["red", "blue"] })).await,
            ["m1", "m2", "p1"]
        );
        assert_eq!(
            search(json!({ "extensions": ["png", "fbx"] })).await,
            ["m2", "p1", "p2"]
        );
        assert_eq!(search(json!({ "isFavorite": true })).await, ["m2", "p1"]);
        assert_eq!(
            search(json!({ "isFavorite": false })).await,
            ["m1", "m3", "p2"]
        );
        assert_eq!(
            search(json!({ "previewStatus": "missing" })).await,
            ["m1", "p2"]
        );
        assert_eq!(
            search(json!({ "previewStatus": "needs_preview" })).await,
            ["m1", "p2"]
        );
        assert_eq!(
            search(json!({ "previewStatus": "has_preview" })).await,
            ["m2", "p1"]
        );
        assert_eq!(
            search(json!({ "previewStatus": "processing" })).await,
            ["m3"]
        );
        assert_eq!(
            search(json!({ "previewStatus": "" })).await,
            ["m1", "m2", "m3", "p1", "p2"]
        );
        assert_eq!(
            search(json!({ "rating": { "min": 2, "max": 3 } })).await,
            ["m3", "p2"]
        );
        assert_eq!(
            search(json!({ "dateRange": { "from": 20, "to": 40 } })).await,
            ["m1", "m2", "p2"]
        );
        assert_eq!(
            search(json!({ "sizeRange": { "min": 250, "max": 450 } })).await,
            ["m1", "m2"]
        );
        assert_eq!(
            search(json!({ "directory": " textures " })).await,
            ["p1", "p2"]
        );
        assert_eq!(
            search(json!({ "kinds": [], "colorLabels": [], "extensions": [] })).await,
            ["m1", "m2", "m3", "p1", "p2"]
        );
        assert_eq!(
            search(json!({
                "kinds": ["model"],
                "isFavorite": false,
                "previewStatus": "missing",
                "extensions": ["glb"],
            }))
            .await,
            ["m1"]
        );
    }

    #[tokio::test]
    async fn a_later_page_skips_the_earlier_pages() {
        let pool = attribute_library().await;
        let page = |query, page, size| search_page(std::slice::from_ref(&pool), query, page, size);
        let all = serde_json::json!({ "filter": {} });
        let models = serde_json::json!({ "filter": { "kinds": ["model"] } });

        assert_eq!(page(all.clone(), 0, 2).await, (ids(&["m1", "m2"]), 5));
        assert_eq!(page(all.clone(), 1, 2).await, (ids(&["m3", "p1"]), 5));
        assert_eq!(page(all, 2, 2).await, (ids(&["p2"]), 5));
        assert_eq!(page(models.clone(), 1, 2).await, (ids(&["m3"]), 3));
        assert_eq!(page(models, 2, 2).await, (vec![], 3));
    }

    #[tokio::test]
    async fn a_later_page_over_several_libraries_skips_the_earlier_pages() {
        let a = attribute_library().await;
        let b = attribute_library().await;
        let both = [a, b];
        let all = serde_json::json!({ "filter": {} });

        assert_eq!(
            search_page(&both, all.clone(), 1, 3).await,
            (ids(&["m2", "m3", "m3"]), 10)
        );
        assert_eq!(search_page(&both, all, 3, 3).await, (ids(&["p2"]), 10));
    }

    /// A library of `count` assets whose sort keys all tie, except that names repeat in threes
    /// and differ only in case within a pair (`Rock.png`, `rock.png`, `tree.png`).
    async fn tied_library(count: usize) -> Pool<Sqlite> {
        let pool = memory_library_pool().await;
        for i in 0..count {
            let name = ["Rock.png", "rock.png", "tree.png"][i % 3];
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
                 VALUES (?, ?, ?, 'png', 'texture', 0, 0, 0)",
            )
            .bind(format!("a{i:04}"))
            .bind(format!("{i}/{name}"))
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    async fn search_sorted(
        pool: &Pool<Sqlite>,
        field: &str,
        order: &str,
        page: i64,
        page_size: i64,
    ) -> SearchResult {
        let payload: SearchPayload = serde_json::from_value(serde_json::json!({
            "query": { "text": "", "filter": {}, "sort": { "field": field, "order": order } },
            "page": page,
            "pageSize": page_size,
        }))
        .unwrap();
        search_libraries(&payload, &[(pool.clone(), None)])
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn consecutive_pages_of_tied_assets_neither_overlap_nor_skip() {
        let count = 600;
        let pool = tied_library(count).await;
        for field in ["date", "name", "size", "rating", "kind", "favorite"] {
            for order in ["asc", "desc"] {
                // Ties come in insertion (rowid) order, in the sort direction.
                let mut expected: Vec<usize> = (0..count).collect();
                if field == "name" {
                    expected.sort_by_key(|i| (*i % 3 == 2, *i));
                }
                if order == "desc" {
                    expected.reverse();
                }
                let expected: Vec<String> = expected.iter().map(|i| format!("a{i:04}")).collect();

                let mut pages = Vec::new();
                for page in 0..(count as i64 / 50) {
                    let result = search_sorted(&pool, field, order, page, 50).await;
                    pages.extend(result.assets.into_iter().map(|a| a.id));
                }
                assert!(
                    pages == expected,
                    "pages sorted by {field} {order} are out of order, overlap or skip"
                );
            }
        }
    }

    #[tokio::test]
    async fn name_order_ignores_case() {
        let pool = tied_library(6).await;
        let names = |result: SearchResult| -> Vec<(String, String)> {
            result
                .assets
                .into_iter()
                .map(|a| (a.file_name, a.id))
                .collect()
        };
        let pairs = |list: &[(&str, &str)]| -> Vec<(String, String)> {
            list.iter()
                .map(|(n, i)| (n.to_string(), i.to_string()))
                .collect()
        };

        assert_eq!(
            names(search_sorted(&pool, "name", "asc", 0, 100).await),
            pairs(&[
                ("Rock.png", "a0000"),
                ("rock.png", "a0001"),
                ("Rock.png", "a0003"),
                ("rock.png", "a0004"),
                ("tree.png", "a0002"),
                ("tree.png", "a0005"),
            ])
        );
        assert_eq!(
            names(search_sorted(&pool, "name", "desc", 0, 100).await),
            pairs(&[
                ("tree.png", "a0005"),
                ("tree.png", "a0002"),
                ("rock.png", "a0004"),
                ("Rock.png", "a0003"),
                ("rock.png", "a0001"),
                ("Rock.png", "a0000"),
            ])
        );
    }

    #[tokio::test]
    async fn sorting_by_name_or_size_reads_an_index_like_sorting_by_date() {
        let pool = memory_library_pool().await;
        for (field, index) in [
            ("date", "idx_assets_modified_at"),
            ("name", "idx_assets_file_name_nocase"),
            ("size", "idx_assets_size_bytes"),
        ] {
            for is_asc in [true, false] {
                let sql = format!(
                    "EXPLAIN QUERY PLAN SELECT a.id FROM assets a ORDER BY {} LIMIT 200 OFFSET 400",
                    order_by(field, is_asc)
                );
                let plan: Vec<String> = sqlx::query(&sql)
                    .fetch_all(&pool)
                    .await
                    .unwrap()
                    .iter()
                    .map(|row| row.get::<String, _>("detail"))
                    .collect();
                assert_eq!(
                    plan,
                    [format!("SCAN a USING INDEX {index}")],
                    "sorting by {field}, ascending {is_asc}"
                );
            }
        }
    }

    /// A library holding `names`, in that order, whose other sort keys repeat so that many tie.
    async fn mixed_library(prefix: &str, names: &[&str]) -> Pool<Sqlite> {
        let pool = memory_library_pool().await;
        for (i, name) in names.iter().enumerate() {
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at,
                    indexed_at, rating, is_favorite)
                 VALUES (?, ?, ?, 'png', ?, ?, ?, 0, ?, ?)",
            )
            .bind(format!("{prefix}{i:02}"))
            .bind(format!("{i}/{name}"))
            .bind(name)
            .bind(["model", "texture", "Model", "sprite"][i % 4])
            .bind((i % 3) as i64 * 1024)
            .bind((i % 5) as i64)
            .bind((i % 2) as i64)
            .bind((i % 3 == 0) as i64)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    /// The ids of every asset in `libs` sorted by `field` in `order`, as one SQLite query over all
    /// of them sorts them, ties broken by library position and then rowid.
    async fn sorted_by_sqlite(libs: &[Pool<Sqlite>], field: &str, order: &str) -> Vec<String> {
        let merged = memory_library_pool().await;
        sqlx::query(
            "CREATE TABLE merged (lib INTEGER, rid INTEGER, id TEXT, file_name TEXT, kind TEXT,
                size_bytes INTEGER, modified_at INTEGER, rating INTEGER, is_favorite INTEGER)",
        )
        .execute(&merged)
        .await
        .unwrap();
        for (lib, pool) in libs.iter().enumerate() {
            let rows = sqlx::query(
                "SELECT rowid, id, file_name, kind, size_bytes, modified_at, rating, is_favorite
                 FROM assets",
            )
            .fetch_all(pool)
            .await
            .unwrap();
            for row in rows {
                sqlx::query("INSERT INTO merged VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
                    .bind(lib as i64)
                    .bind(row.get::<i64, _>(0))
                    .bind(row.get::<String, _>(1))
                    .bind(row.get::<String, _>(2))
                    .bind(row.get::<String, _>(3))
                    .bind(row.get::<i64, _>(4))
                    .bind(row.get::<i64, _>(5))
                    .bind(row.get::<i64, _>(6))
                    .bind(row.get::<i64, _>(7))
                    .execute(&merged)
                    .await
                    .unwrap();
            }
        }
        let dir = if order == "asc" { "ASC" } else { "DESC" };
        sqlx::query_scalar(&format!(
            "SELECT id FROM merged a ORDER BY {} {dir}, a.lib ASC, a.rid {dir}",
            sort_column(field).0
        ))
        .fetch_all(&merged)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn pages_over_several_libraries_match_one_full_sort() {
        // Case variants, non-ASCII letters that only Unicode folds (`Ä`/`ä`, `É`/`é`) and
        // punctuation between the ASCII cases, with names repeated within and across libraries.
        let names = [
            "Ä_b.png",
            "ä_a.png",
            "rock.png",
            "Rock.png",
            "ROCK.png",
            "zebra.png",
            "Éclair.png",
            "éclair.png",
            "_under.png",
            "[bracket].png",
            "Straße.png",
            "STRASSE.png",
            "rock.png",
            "Zebra.png",
            "a.png",
        ];
        let reversed: Vec<&str> = names.iter().rev().copied().collect();
        let libs = [
            mixed_library("a", &names).await,
            mixed_library("b", &reversed[..10]).await,
        ];
        let targets: Vec<_> = libs.iter().map(|pool| (pool.clone(), None)).collect();
        let count = names.len() + 10;

        for field in ["name", "kind", "size", "modified", "rating", "favorite"] {
            for order in ["asc", "desc"] {
                let expected = sorted_by_sqlite(&libs, field, order).await;
                for page_size in [1, 4, 7, count as i64] {
                    let mut found = Vec::new();
                    // One page past the end, which is empty.
                    for page in 0..=(count as i64 / page_size) {
                        let payload: SearchPayload = serde_json::from_value(serde_json::json!({
                            "query": {
                                "text": "", "filter": {}, "sort": { "field": field, "order": order }
                            },
                            "page": page,
                            "pageSize": page_size,
                            "includeTotal": false,
                        }))
                        .unwrap();
                        let result = search_libraries(&payload, &targets).await.unwrap();
                        let start = (page * page_size) as usize;
                        let end = (start + page_size as usize).min(count);
                        let ids: Vec<String> = result.assets.into_iter().map(|a| a.id).collect();
                        assert_eq!(
                            ids,
                            expected[start..end],
                            "{field} {order}, page {page} of {page_size}"
                        );
                        found.extend(ids);
                    }
                    assert_eq!(found, expected, "{field} {order}, pages of {page_size}");
                }
            }
        }
    }

    #[tokio::test]
    async fn an_asset_deleted_between_the_key_and_row_reads_stays_on_the_page() {
        let dir = tempfile::tempdir().unwrap();
        let mut libs = Vec::new();
        for lib in ["a", "b"] {
            let path = dir.path().join(format!("{lib}.sqlite"));
            let pool = crate::db::connect_library_sqlite(&path).await.unwrap();
            for name in ["x", "y"] {
                sqlx::query(
                    "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
                     VALUES (?, ?, ?, 'png', 'texture', 0, 0, 0)",
                )
                .bind(format!("{lib}-{name}"))
                .bind(format!("{name}.png"))
                .bind(format!("{name}.png"))
                .execute(&pool)
                .await
                .unwrap();
            }
            libs.push((path, pool));
        }
        // A writer on its own connection, as the watcher is, that waits for no lock.
        let delete = |path: PathBuf, id: &'static str| {
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        let options = sqlx::sqlite::SqliteConnectOptions::new()
                            .filename(&path)
                            .busy_timeout(std::time::Duration::ZERO);
                        let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
                        sqlx::query("DELETE FROM assets WHERE id = ?")
                            .bind(id)
                            .execute(&pool)
                            .await
                            .is_ok()
                    })
            })
            .join()
            .unwrap()
        };
        let deleted_mid_search = std::rc::Rc::new(std::cell::Cell::new(None));
        let (path_a, deleted) = (libs[0].0.clone(), deleted_mid_search.clone());
        BETWEEN_KEYS_AND_ROWS.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || deleted.set(Some(delete(path_a, "a-y")))))
        });

        let payload: SearchPayload = serde_json::from_value(serde_json::json!({
            "query": { "text": "", "filter": {}, "sort": { "field": "name", "order": "asc" } },
            "page": 0,
            "pageSize": 4,
        }))
        .unwrap();
        let targets: Vec<_> = libs.iter().map(|(_, pool)| (pool.clone(), None)).collect();
        let result = search_libraries(&payload, &targets).await.unwrap();

        assert_eq!(
            deleted_mid_search.get(),
            Some(false),
            "a write landed between the key and row reads"
        );
        let found: Vec<String> = result.assets.into_iter().map(|a| a.id).collect();
        assert_eq!(found, ["a-x", "b-x", "a-y", "b-y"]);
        assert!(
            delete(libs[0].0.clone(), "a-y"),
            "the search left its read lock held"
        );
    }

    #[tokio::test]
    async fn a_search_without_the_total_skips_the_count() {
        let a = attribute_library().await;
        let b = attribute_library().await;
        let payload = |include_total: Option<bool>| {
            let mut payload = serde_json::json!({
                "query": { "text": "", "filter": {}, "sort": { "field": "name", "order": "asc" } },
                "page": 0,
                "pageSize": 2,
            });
            if let Some(include) = include_total {
                payload["includeTotal"] = include.into();
            }
            serde_json::from_value::<SearchPayload>(payload).unwrap()
        };
        // The first page and every match, over one library and over two.
        let cases = [
            (vec![(a.clone(), None)], ids(&["m1", "m2"]), 5),
            (vec![(a, None), (b, None)], ids(&["m1", "m1"]), 10),
        ];

        for (libs, page, total) in cases {
            let search = |include_total| {
                let libs = &libs;
                async move {
                    let before = COUNT_QUERIES.with(|c| c.get());
                    let result = search_libraries(&payload(include_total), libs)
                        .await
                        .unwrap();
                    (result, COUNT_QUERIES.with(|c| c.get()) - before)
                }
            };
            let n = libs.len();

            let (without, counts) = search(Some(false)).await;
            assert_eq!(counts, 0, "{n} libraries counted without includeTotal");
            assert_eq!(without.total, None);
            assert!(serde_json::to_value(&without)
                .unwrap()
                .get("total")
                .is_none());
            let found: Vec<String> = without.assets.into_iter().map(|a| a.id).collect();
            assert_eq!(found, page);

            for include_total in [Some(true), None] {
                let (with, counts) = search(include_total).await;
                assert_eq!(counts, n, "includeTotal {include_total:?}");
                assert_eq!(with.total, Some(total), "includeTotal {include_total:?}");
            }
        }
    }

    async fn tagged(pool: &sqlx::Pool<sqlx::Sqlite>) -> Vec<(String, String)> {
        sqlx::query_as("SELECT asset_id, tag_id FROM asset_tags ORDER BY asset_id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn tagging_assets_writes_each_membership_only_to_the_assets_own_library() {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        insert_asset(&a, "in-a").await;
        insert_asset(&b, "in-b").await;
        let db = memory_database(vec![
            memory_library("a", a.clone()),
            memory_library("b", b.clone()),
        ])
        .await;
        for pool in [&db.master_pool, &a, &b] {
            insert_tag(pool, "t", "rock").await;
        }
        let patch: AssetPatch =
            serde_json::from_value(serde_json::json!({ "tags": ["t"] })).unwrap();

        apply_asset_patch(&db, "in-a", &patch).await.unwrap();
        apply_asset_patch(&db, "in-b", &patch).await.unwrap();

        assert_eq!(tagged(&a).await, [("in-a".to_string(), "t".to_string())]);
        assert_eq!(tagged(&b).await, [("in-b".to_string(), "t".to_string())]);
    }

    #[tokio::test]
    async fn tagging_an_asset_with_a_tag_its_mirror_lacks_records_the_tag() {
        let a = memory_library_pool_enforcing_foreign_keys().await;
        insert_asset(&a, "in-a").await;
        let db = memory_database(vec![memory_library("a", a.clone())]).await;
        insert_tag(&db.master_pool, "t", "rock").await;
        let patch: AssetPatch =
            serde_json::from_value(serde_json::json!({ "tags": ["t"] })).unwrap();

        apply_asset_patch(&db, "in-a", &patch).await.unwrap();

        assert_eq!(tagged(&a).await, [("in-a".to_string(), "t".to_string())]);
    }

    #[tokio::test]
    async fn tagging_with_a_child_tag_copies_its_missing_ancestors_into_the_mirror() {
        let a = memory_library_pool_enforcing_foreign_keys().await;
        insert_asset(&a, "in-a").await;
        let db = memory_database(vec![memory_library("a", a.clone())]).await;
        insert_tag(&db.master_pool, "root", "nature").await;
        for (id, name, parent) in [("mid", "plants", "root"), ("leaf", "trees", "mid")] {
            sqlx::query("INSERT INTO tags (id, name, parent_id) VALUES (?, ?, ?)")
                .bind(id)
                .bind(name)
                .bind(parent)
                .execute(&db.master_pool)
                .await
                .unwrap();
        }
        let patch: AssetPatch =
            serde_json::from_value(serde_json::json!({ "tags": ["leaf"] })).unwrap();

        apply_asset_patch(&db, "in-a", &patch).await.unwrap();

        assert_eq!(tagged(&a).await, [("in-a".to_string(), "leaf".to_string())]);
        let mirrored: Vec<String> = sqlx::query_scalar("SELECT id FROM tags ORDER BY id")
            .fetch_all(&a)
            .await
            .unwrap();
        assert_eq!(mirrored, ["leaf", "mid", "root"]);
    }

    #[tokio::test]
    async fn a_patch_whose_tags_fail_saves_none_of_its_fields() {
        let a = memory_library_pool_enforcing_foreign_keys().await;
        insert_asset(&a, "in-a").await;
        insert_tag(&a, "other", "rock").await;
        let db = memory_database(vec![memory_library("a", a.clone())]).await;
        insert_tag(&db.master_pool, "t", "rock").await;
        let patch: AssetPatch =
            serde_json::from_value(serde_json::json!({ "rating": 5, "tags": ["t"] })).unwrap();

        let result = apply_asset_patch(&db, "in-a", &patch).await;

        assert!(result.is_err());
        let rating: i64 = sqlx::query_scalar("SELECT rating FROM assets WHERE id = 'in-a'")
            .fetch_one(&a)
            .await
            .unwrap();
        assert_eq!(rating, 0);
    }

    #[tokio::test]
    async fn tagging_with_a_tag_whose_name_collides_in_the_mirror_is_an_error() {
        let a = memory_library_pool_enforcing_foreign_keys().await;
        insert_asset(&a, "in-a").await;
        insert_tag(&a, "other", "rock").await;
        let db = memory_database(vec![memory_library("a", a.clone())]).await;
        insert_tag(&db.master_pool, "t", "rock").await;
        let patch: AssetPatch =
            serde_json::from_value(serde_json::json!({ "tags": ["t"] })).unwrap();

        let result = apply_asset_patch(&db, "in-a", &patch).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn patching_an_asset_in_no_open_library_is_an_error_and_writes_nothing() {
        let a = memory_library_pool().await;
        insert_tag(&a, "t", "rock").await;
        let db = memory_database(vec![memory_library("a", a.clone())]).await;
        let patch: AssetPatch =
            serde_json::from_value(serde_json::json!({ "tags": ["t"] })).unwrap();

        let result = apply_asset_patch(&db, "nowhere", &patch).await;

        assert!(result.is_err());
        assert!(tagged(&a).await.is_empty());
        assert!(tagged(&db.master_pool).await.is_empty());
    }

    /// A library in a temporary root holding `<name>.glb` for each name, indexed as asset
    /// `<name>`. Returns the root, the database and the library's pool.
    async fn library_with(names: &[&str]) -> (PathBuf, Database, Pool<Sqlite>) {
        let root = std::env::temp_dir().join(format!("opendam-trash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pool = memory_library_pool().await;
        for name in names {
            std::fs::write(root.join(format!("{name}.glb")), "glTF").unwrap();
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
                 VALUES (?, ?, ?, 'glb', 'model', 4, 0, 0)",
            )
            .bind(name)
            .bind(format!("{name}.glb"))
            .bind(format!("{name}.glb"))
            .execute(&pool)
            .await
            .unwrap();
        }
        let library = LibraryHandle {
            id: "lib".to_string(),
            name: "lib".to_string(),
            root_path: root.clone(),
            pool: pool.clone(),
        };
        let db = memory_database(vec![library]).await;
        (root, db, pool)
    }

    fn fs_rename(from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::rename(from, to)
    }

    async fn has_row(pool: &Pool<Sqlite>, id: &str) -> bool {
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM assets WHERE id = ?")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
        rows == 1
    }

    /// A trash function that fails for files named `<name>.glb` for any of `names`.
    fn trash_failing_for(
        names: &'static [&'static str],
    ) -> impl Fn(&Path) -> std::result::Result<(), &'static str> {
        move |path| {
            let stem = path.file_stem().unwrap().to_str().unwrap();
            if names.contains(&stem) {
                Err("trash unavailable")
            } else {
                Ok(())
            }
        }
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn trash_error(root: &Path, name: &str) -> String {
        format!(
            "Could not move {} to the trash, so it was not deleted: trash unavailable",
            root.join(format!("{name}.glb")).display()
        )
    }

    #[tokio::test]
    async fn a_failed_trash_keeps_the_file_and_the_asset_row() {
        let (root, db, pool) = library_with(&["tree"]).await;

        let result = delete_assets_with(&db, &ids(&["tree"]), true, |_| {
            Err::<(), _>("trash unavailable")
        })
        .await;

        let file_exists = root.join("tree.glb").exists();
        let row_kept = has_row(&pool, "tree").await;
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            result.map_err(|e| e.message),
            Err(trash_error(&root, "tree"))
        );
        assert!(file_exists);
        assert!(row_kept);
    }

    #[tokio::test]
    async fn a_successful_trash_trashes_the_file_and_deletes_the_asset_row() {
        let (root, db, pool) = library_with(&["tree"]).await;
        let trashed = RefCell::new(Vec::new());

        let result = delete_assets_with(&db, &ids(&["tree"]), true, |path| {
            trashed.borrow_mut().push(path.to_path_buf());
            Ok::<(), &str>(())
        })
        .await;

        let row_kept = has_row(&pool, "tree").await;
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(result, Ok(()));
        assert_eq!(trashed.into_inner(), vec![root.join("tree.glb")]);
        assert!(!row_kept);
    }

    /// A library in a temporary root holding a file at each of `paths`, indexed as an asset whose
    /// id is its path and whose extension is its path's, with its first structure pass run.
    async fn library_in_folders(paths: &[&str]) -> (PathBuf, Database, Pool<Sqlite>) {
        let (root, db, pool) = library_with(&[]).await;
        for path in paths {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "glTF").unwrap();
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
                 VALUES (?, ?, ?, ?, 'model', 4, 0, 0)",
            )
            .bind(path)
            .bind(path)
            .bind(path.rsplit('/').next().unwrap())
            .bind(path.rsplit('.').next().unwrap())
            .execute(&pool)
            .await
            .unwrap();
        }
        crate::structure::rebuild(&pool, &root).await.unwrap();
        (root, db, pool)
    }

    const FOLDER_FILES: [&str; 4] = [
        "3D assets/Platformer Kit/Models/block.glb",
        "3D assets/Platformer Kit/Models/crate.glb",
        "3D assets/Nature Kit/Rocks/rock.glb",
        "tree.glb",
    ];

    #[tokio::test]
    async fn deleting_assets_leaves_the_folders_a_fresh_pass_would() {
        let (root, db, pool) = library_in_folders(&FOLDER_FILES).await;

        // The rock empties `Nature Kit` and `Nature Kit/Rocks`.
        let deleted = ids(&[
            "3D assets/Platformer Kit/Models/crate.glb",
            "3D assets/Nature Kit/Rocks/rock.glb",
        ]);
        let result = delete_assets_with(&db, &deleted, true, trash_failing_for(&[])).await;

        let folders = crate::structure::tests::derived_rows(&pool).await;
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(result, Ok(()));
        let rest = [FOLDER_FILES[0], FOLDER_FILES[3]];
        assert_eq!(folders, crate::structure::tests::fresh_pass(&rest).await);
        assert!(!folders
            .folders
            .iter()
            .any(|f| f.0.starts_with("3D assets/Nature Kit")));
    }

    #[tokio::test]
    async fn renaming_an_asset_leaves_the_folders_a_fresh_pass_would() {
        let (root, db, pool) = library_in_folders(&FOLDER_FILES).await;

        let result = rename_asset_with(&db, FOLDER_FILES[2], "boulder", fs_rename).await;

        let folders = crate::structure::tests::derived_rows(&pool).await;
        let renamed = root.join("3D assets/Nature Kit/Rocks/boulder.glb").exists();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(result, Ok(()));
        assert!(renamed);
        let after = [
            FOLDER_FILES[0],
            FOLDER_FILES[1],
            "3D assets/Nature Kit/Rocks/boulder.glb",
            FOLDER_FILES[3],
        ];
        assert_eq!(folders, crate::structure::tests::fresh_pass(&after).await);
    }

    #[tokio::test]
    async fn renaming_or_deleting_a_packs_marker_leaves_the_packs_a_fresh_pass_would() {
        let files = [
            "kits/Car Kit/License.txt",
            "kits/Car Kit/Readme.txt",
            "kits/Car Kit/sedan.glb",
        ];
        let (root, db, pool) = library_in_folders(&files).await;
        let before = crate::structure::tests::derived_rows(&pool).await;

        let renamed = rename_asset_with(&db, files[0], "Notes", fs_rename).await;
        let after_rename = crate::structure::tests::derived_rows(&pool).await;
        let deleted =
            delete_assets_with(&db, &ids(&[files[1]]), true, trash_failing_for(&[])).await;
        let after_delete = crate::structure::tests::derived_rows(&pool).await;
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!((renamed, deleted), (Ok(()), Ok(())));
        assert_eq!(before, crate::structure::tests::fresh_pass(&files).await);
        assert!(before.folders.iter().any(|f| f.0 == "kits/Car Kit" && f.3));
        // Without its license the kit is no pack, so its docs show again.
        let renamed = ["kits/Car Kit/Notes.txt", files[1], files[2]];
        assert_eq!(
            after_rename,
            crate::structure::tests::fresh_pass(&renamed).await
        );
        assert!(after_rename.roles.iter().all(|(_, role)| role.is_none()));
        assert_eq!(
            after_delete,
            crate::structure::tests::fresh_pass(&[renamed[0], renamed[2]]).await
        );
    }

    #[tokio::test]
    async fn a_rename_the_library_db_refuses_is_undone_on_disk() {
        let (root, db, pool) = library_in_folders(&FOLDER_FILES).await;
        sqlx::query(
            "CREATE TRIGGER refuse BEFORE UPDATE ON assets BEGIN SELECT RAISE(ABORT, 'busy'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let result = rename_asset_with(&db, FOLDER_FILES[3], "oak", fs_rename).await;

        let kept = root.join("tree.glb").exists();
        let renamed = root.join("oak.glb").exists();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(result.unwrap_err().contains("so it was undone"));
        assert!(kept && !renamed);
    }

    #[tokio::test]
    async fn deleting_several_assets_succeeds_when_every_trash_does() {
        let (root, db, pool) = library_with(&["tree", "rock"]).await;

        let result =
            delete_assets_with(&db, &ids(&["tree", "rock"]), true, trash_failing_for(&[])).await;

        let rows_kept = [has_row(&pool, "tree").await, has_row(&pool, "rock").await];
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(result, Ok(()));
        assert_eq!(rows_kept, [false, false]);
    }

    #[tokio::test]
    async fn deleting_several_assets_returns_the_one_failed_trash() {
        let (root, db, pool) = library_with(&["tree", "rock"]).await;

        let result = delete_assets_with(
            &db,
            &ids(&["tree", "rock"]),
            true,
            trash_failing_for(&["rock"]),
        )
        .await;

        let rows_kept = [has_row(&pool, "tree").await, has_row(&pool, "rock").await];
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            result,
            Err(DeleteAssetsError {
                message: trash_error(&root, "rock"),
                failed_ids: ids(&["rock"]),
            })
        );
        assert_eq!(rows_kept, [false, true]);
    }

    #[tokio::test]
    async fn deleting_several_assets_counts_the_failed_trashes() {
        let (root, db, pool) = library_with(&["tree", "rock", "bush"]).await;

        let result = delete_assets_with(
            &db,
            &ids(&["tree", "rock", "bush"]),
            true,
            trash_failing_for(&["tree", "bush"]),
        )
        .await;

        let rows_kept = [
            has_row(&pool, "tree").await,
            has_row(&pool, "rock").await,
            has_row(&pool, "bush").await,
        ];
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            result,
            Err(DeleteAssetsError {
                message: format!("2 files were not deleted. {}", trash_error(&root, "tree")),
                failed_ids: ids(&["tree", "bush"]),
            })
        );
        assert_eq!(rows_kept, [true, false, true]);
    }

    #[test]
    fn delete_assets_error_serializes_as_the_frontend_expects() {
        let error = DeleteAssetsError {
            message: "2 files were not deleted.".to_string(),
            failed_ids: ids(&["tree", "bush"]),
        };
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            serde_json::json!({
                "message": "2 files were not deleted.",
                "failedIds": ["tree", "bush"],
            })
        );
    }

    /// The Car Kit with the sedan in three formats, Kenney's render of it, and a van on its own.
    fn sedan_kit() -> Vec<&'static str> {
        let mut files = crate::structure::tests::SEDAN.to_vec();
        files.push(crate::structure::tests::SEDAN_RENDER);
        files.push("3D assets/Car Kit/Models/GLB format/van.glb");
        files
    }

    /// The sedan's main item, the GLB, which is the card a delete or rename starts from.
    const SEDAN_GLB: &str = "3D assets/Car Kit/Models/GLB format/sedan.glb";

    /// The sedan's four files, its render included, renamed to `stem`.
    fn sedan_files(stem: &str) -> Vec<String> {
        let mut files: Vec<String> = crate::structure::tests::SEDAN
            .iter()
            .chain([&crate::structure::tests::SEDAN_RENDER])
            .map(|path| path.replace("/sedan.", &format!("/{stem}.")))
            .collect();
        files.sort();
        files
    }

    /// The sedan's files that are on disk under `root` for each of `stems`, sorted by path.
    fn on_disk(root: &Path, stems: &[&str]) -> Vec<String> {
        let mut files: Vec<String> = stems
            .iter()
            .flat_map(|stem| sedan_files(stem))
            .filter(|path| root.join(path).exists())
            .collect();
        files.sort();
        files
    }

    /// Makes the sedan's render under `root` Kenney's 64 px one, and runs the pass, which reads
    /// it as too small to picture the sedan.
    async fn small_sedan_render(root: &Path, pool: &Pool<Sqlite>) {
        use crate::structure::tests::{render_read, write_png, SEDAN_RENDER};
        write_png(&root.join(SEDAN_RENDER), 64, 64);
        crate::structure::rebuild(pool, root).await.unwrap();
        assert_eq!(
            render_read(pool, SEDAN_RENDER).await[..2],
            [Some(64), Some(64)]
        );
        let pictured: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM assets a WHERE {}",
            pictured("a")
        ))
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(pictured, 0);
    }

    /// Registers the test library and another one in master, each counting 99 assets.
    async fn register_libraries(db: &Database) {
        sqlx::query(
            "INSERT INTO libraries (id, name, root_paths, created_at, asset_count)
             VALUES ('lib', 'lib', '[]', 0, 99), ('other', 'other', '[]', 0, 99)",
        )
        .execute(&db.master_pool)
        .await
        .unwrap();
    }

    /// Each library's asset count in master, by id.
    async fn asset_counts(db: &Database) -> Vec<(String, i64)> {
        sqlx::query_as("SELECT id, asset_count FROM libraries ORDER BY id")
            .fetch_all(&db.master_pool)
            .await
            .unwrap()
    }

    /// `(id, count)` for the test library and the other one, which keeps its 99.
    fn counts(lib: i64) -> Vec<(String, i64)> {
        vec![("lib".to_string(), lib), ("other".to_string(), 99)]
    }

    #[tokio::test]
    async fn renaming_a_group_renames_every_member_and_its_render() {
        let files = sedan_kit();
        let (root, db, pool) = library_in_folders(&files).await;
        // A render too small to picture the sedan is still its render, and moves with it.
        small_sedan_render(&root, &pool).await;

        let result = rename_asset_with(&db, SEDAN_GLB, "coupe.glb", fs_rename).await;

        let derived = crate::structure::tests::derived_rows(&pool).await;
        let disk = on_disk(&root, &["sedan", "coupe"]);
        let van_kept = root.join(files[4]).exists();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(result, Ok(()));
        assert_eq!(disk, sedan_files("coupe"));
        assert!(van_kept);
        let mut after = sedan_files("coupe");
        after.push(files[4].to_string());
        let after: Vec<&str> = after.iter().map(String::as_str).collect();
        assert_eq!(derived, crate::structure::tests::fresh_pass(&after).await);
        assert_eq!(
            derived.bundled_previews,
            [(
                "3D assets/Car Kit/Models/GLB format/coupe.glb".to_string(),
                "3D assets/Car Kit/Previews/coupe.png".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn deleting_a_group_trashes_every_member_and_its_render() {
        let files = sedan_kit();
        let (root, db, pool) = library_in_folders(&files).await;
        small_sedan_render(&root, &pool).await;
        register_libraries(&db).await;
        let trashed = RefCell::new(Vec::new());

        let result = delete_assets_with(&db, &ids(&[SEDAN_GLB]), true, |path| {
            trashed.borrow_mut().push(make_relative_path(&root, path));
            Ok::<(), &str>(())
        })
        .await;

        let derived = crate::structure::tests::derived_rows(&pool).await;
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(result, Ok(()));
        // Only the van is left, and only this library's count changes.
        assert_eq!(asset_counts(&db).await, counts(1));
        // The main item first, the other members by path, then the render.
        assert_eq!(
            trashed.into_inner(),
            [
                SEDAN_GLB,
                "3D assets/Car Kit/Models/FBX format/sedan.fbx",
                "3D assets/Car Kit/Models/OBJ format/sedan.obj",
                "3D assets/Car Kit/Previews/sedan.png",
            ]
        );
        assert_eq!(
            derived,
            crate::structure::tests::fresh_pass(&[files[4]]).await
        );
    }

    #[tokio::test]
    async fn a_failed_trash_in_a_group_keeps_the_files_after_it_and_names_them() {
        let files = sedan_kit();
        let (root, db, pool) = library_in_folders(&files).await;
        register_libraries(&db).await;

        let result = delete_assets_with(&db, &ids(&[SEDAN_GLB]), true, |path| {
            if path.extension().is_some_and(|e| e == "obj") {
                Err("trash unavailable")
            } else {
                Ok(())
            }
        })
        .await;

        let derived = crate::structure::tests::derived_rows(&pool).await;
        std::fs::remove_dir_all(&root).unwrap();
        let obj = "3D assets/Car Kit/Models/OBJ format/sedan.obj";
        let render = crate::structure::tests::SEDAN_RENDER;
        assert_eq!(
            result,
            Err(DeleteAssetsError {
                message: format!(
                    "Could not move {} to the trash, so it was not deleted: trash unavailable. \
                     Still in place: {obj}, {render}",
                    root.join(obj).display()
                ),
                failed_ids: ids(&[SEDAN_GLB]),
            })
        );
        // The GLB and FBX lost their rows, and the pass ran on what is left.
        assert_eq!(asset_counts(&db).await, counts(3));
        let rest = [obj, render, files[4]];
        assert_eq!(derived, crate::structure::tests::fresh_pass(&rest).await);
    }

    #[tokio::test]
    async fn a_render_two_items_show_goes_only_with_the_last_of_them() {
        let files = [
            "Kit/Previews/tree.png",
            "Kit/Models/tree.glb",
            "Kit/Props/tree.glb",
        ];
        let (root, db, pool) = library_in_folders(&files).await;
        let before = crate::structure::tests::derived_rows(&pool).await;

        // The render is the models' both, so it stays when one goes, and goes with the other.
        let deleted = delete_assets_with(&db, &ids(&[files[2]]), true, |path| {
            std::fs::remove_file(path)
        })
        .await;
        let render_kept = root.join(files[0]).exists();
        let renamed = rename_asset_with(&db, files[1], "oak", fs_rename).await;

        let derived = crate::structure::tests::derived_rows(&pool).await;
        let render_renamed = root.join("Kit/Previews/oak.png").exists();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(before.bundled_previews.len(), 2);
        assert_eq!((deleted, renamed), (Ok(()), Ok(())));
        assert!(render_kept && render_renamed);
        let after = ["Kit/Previews/oak.png", "Kit/Models/oak.glb"];
        assert_eq!(derived, crate::structure::tests::fresh_pass(&after).await);
    }

    #[tokio::test]
    async fn a_file_on_its_own_with_a_render_can_change_format_and_takes_its_render_along() {
        let files = ["Kit/Models/rock.glb", "Kit/Previews/rock.png"];
        let (root, db, pool) = library_in_folders(&files).await;

        let result = rename_asset_with(&db, files[0], "boulder.fbx", fs_rename).await;

        let derived = crate::structure::tests::derived_rows(&pool).await;
        let extension: String = sqlx::query_scalar("SELECT extension FROM assets WHERE id = ?")
            .bind(files[0])
            .fetch_one(&pool)
            .await
            .unwrap();
        let disk: Vec<bool> = ["Kit/Models/boulder.fbx", "Kit/Previews/boulder.png"]
            .iter()
            .chain(&files)
            .map(|path| root.join(path).exists())
            .collect();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(result, Ok(()));
        assert_eq!(disk, [true, true, false, false]);
        assert_eq!(extension, "fbx");
        let after = ["Kit/Models/boulder.fbx", "Kit/Previews/boulder.png"];
        assert_eq!(derived, crate::structure::tests::fresh_pass(&after).await);
        assert_eq!(
            derived.bundled_previews,
            [(after[0].to_string(), after[1].to_string())]
        );
    }

    #[tokio::test]
    async fn a_group_rename_onto_a_taken_name_moves_nothing() {
        let files = sedan_kit();
        let (root, db, pool) = library_in_folders(&files).await;
        let before = crate::structure::tests::derived_rows(&pool).await;
        let taken = "3D assets/Car Kit/Models/OBJ format/coupe.obj";
        std::fs::write(root.join(taken), "").unwrap();

        let result = rename_asset_with(&db, SEDAN_GLB, "coupe", fs_rename).await;

        let derived = crate::structure::tests::derived_rows(&pool).await;
        let disk = on_disk(&root, &["sedan", "coupe"]);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            result,
            Err(format!(
                "Nothing was renamed, because these names are taken: {taken}"
            ))
        );
        let mut expected = sedan_files("sedan");
        expected.push(taken.to_string());
        expected.sort();
        assert_eq!(disk, expected);
        assert_eq!(derived, before);
    }

    #[tokio::test]
    async fn a_group_rename_that_fails_midway_puts_every_file_back() {
        let files = sedan_kit();
        let (root, db, pool) = library_in_folders(&files).await;
        let before = crate::structure::tests::derived_rows(&pool).await;
        let renamed = RefCell::new(0);

        // The GLB and the FBX are renamed before the OBJ fails.
        let result = rename_asset_with(&db, SEDAN_GLB, "coupe", |from, to| {
            if to.extension().is_some_and(|e| e == "obj") {
                return Err(std::io::Error::other("disk unavailable"));
            }
            *renamed.borrow_mut() += 1;
            std::fs::rename(from, to)
        })
        .await;

        let derived = crate::structure::tests::derived_rows(&pool).await;
        let disk = on_disk(&root, &["sedan", "coupe"]);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            result,
            Err("Failed to rename file on disk: disk unavailable".to_string())
        );
        // Two renames there and two back
        assert_eq!(renamed.into_inner(), 4);
        assert_eq!(disk, sedan_files("sedan"));
        assert_eq!(derived, before);
    }

    #[tokio::test]
    async fn a_group_rename_whose_undo_fails_names_the_files_left_renamed() {
        let files = sedan_kit();
        let (root, db, pool) = library_in_folders(&files).await;
        let before = crate::structure::tests::derived_rows(&pool).await;

        // The OBJ fails after the GLB and the FBX are renamed, and the FBX can't go back.
        let result = rename_asset_with(&db, SEDAN_GLB, "coupe", |from, to| {
            let ext = to.extension().unwrap();
            let back = to.file_stem().unwrap() == "sedan";
            if ext == "obj" || (back && ext == "fbx") {
                return Err(std::io::Error::other("disk unavailable"));
            }
            std::fs::rename(from, to)
        })
        .await;

        let derived = crate::structure::tests::derived_rows(&pool).await;
        let disk = on_disk(&root, &["sedan", "coupe"]);
        std::fs::remove_dir_all(&root).unwrap();
        let fbx = root.join("3D assets/Car Kit/Models/FBX format/coupe.fbx");
        assert_eq!(
            result,
            Err(format!(
                "Failed to rename file on disk (disk unavailable), and could not undo the renames \
                 of: {} (disk unavailable)",
                fbx.display()
            ))
        );
        assert_eq!(
            disk,
            [
                "3D assets/Car Kit/Models/FBX format/coupe.fbx",
                SEDAN_GLB,
                "3D assets/Car Kit/Models/OBJ format/sedan.obj",
                crate::structure::tests::SEDAN_RENDER,
            ]
        );
        // No row was written, so they all keep their old paths.
        assert_eq!(derived, before);
    }

    #[tokio::test]
    async fn a_refused_row_write_whose_undo_fails_names_the_files_left_renamed() {
        let files = sedan_kit();
        let (root, db, pool) = library_in_folders(&files).await;
        sqlx::query(
            "CREATE TRIGGER refuse BEFORE UPDATE ON assets BEGIN SELECT RAISE(ABORT, 'busy'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Every file is renamed, then the render can't go back.
        let result = rename_asset_with(&db, SEDAN_GLB, "coupe", |from, to| {
            if to.extension().unwrap() == "png" && to.file_stem().unwrap() == "sedan" {
                return Err(std::io::Error::other("disk unavailable"));
            }
            std::fs::rename(from, to)
        })
        .await;

        let disk = on_disk(&root, &["sedan", "coupe"]);
        std::fs::remove_dir_all(&root).unwrap();
        let render = root.join("3D assets/Car Kit/Previews/coupe.png");
        assert_eq!(
            result,
            Err(format!(
                "Could not record the rename (error returned from database: (code: 1811) busy), \
                 and could not undo it on disk: {} (disk unavailable)",
                render.display()
            ))
        );
        let mut expected = sedan_files("sedan");
        expected.retain(|path| !path.ends_with(".png"));
        expected.push("3D assets/Car Kit/Previews/coupe.png".to_string());
        expected.sort();
        assert_eq!(disk, expected);
    }

    #[tokio::test]
    async fn a_group_rename_the_library_db_refuses_puts_every_file_back() {
        let files = sedan_kit();
        let (root, db, pool) = library_in_folders(&files).await;
        sqlx::query(
            "CREATE TRIGGER refuse BEFORE UPDATE OF file_path ON assets
             WHEN NEW.file_path LIKE '%.obj' BEGIN SELECT RAISE(ABORT, 'busy'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let result = rename_asset_with(&db, SEDAN_GLB, "coupe", fs_rename).await;

        let paths: Vec<String> =
            sqlx::query_scalar("SELECT file_path FROM assets WHERE file_path LIKE '%sedan.%'")
                .fetch_all(&pool)
                .await
                .unwrap();
        let disk = on_disk(&root, &["sedan", "coupe"]);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(result.unwrap_err().contains("so it was undone"));
        assert_eq!(disk, sedan_files("sedan"));
        // The rows written before the refusal were rolled back with it.
        assert_eq!(paths.len(), 4);
    }

    /// A library holding `paths`, each an asset whose id is its path. Every file has the same
    /// size, rating, kind and date, so every sort but by name breaks ties by rowid alone.
    async fn library_of(id: &str, paths: &[&str]) -> LibraryHandle {
        let pool = memory_library_pool().await;
        for path in paths {
            let name = path.rsplit('/').next().unwrap();
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
                 VALUES (?, ?, ?, 'glb', 'model', 4, 0, 0)",
            )
            .bind(path)
            .bind(path)
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
        }
        LibraryHandle {
            id: id.to_string(),
            name: id.to_string(),
            root_path: PathBuf::from(format!("/libraries/{id}")),
            pool,
        }
    }

    /// The ids of the assets `query` (`text` and `filter` optional) finds in `libraries`, as
    /// `search_assets` targets them, sorted by `field` in `order`.
    async fn folder_search(
        libraries: &[LibraryHandle],
        query: serde_json::Value,
        field: &str,
        order: &str,
    ) -> Vec<String> {
        let mut query = query;
        query["sort"] = serde_json::json!({ "field": field, "order": order });
        query["text"] = query.get("text").cloned().unwrap_or("".into());
        query["filter"] = query
            .get("filter")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let payload: SearchPayload = serde_json::from_value(serde_json::json!({
            "query": query,
            "page": 0,
            "pageSize": 1000,
        }))
        .unwrap();
        let targets = target_libraries(&payload.query, libraries);
        let result = search_libraries(&payload, &targets)
            .await
            .unwrap_or_else(|e| panic!("search for {query} failed: {e}"));
        assert_eq!(result.total, Some(result.assets.len() as i64));
        result.assets.into_iter().map(|a| a.id).collect()
    }

    /// The ids `query` finds in `libraries` sorted by name, with the result's item and file totals.
    async fn counted_search(
        libraries: &[LibraryHandle],
        query: serde_json::Value,
    ) -> (Vec<String>, Option<i64>, Option<i64>) {
        let mut query = query;
        query["sort"] = serde_json::json!({ "field": "name", "order": "asc" });
        query["text"] = query.get("text").cloned().unwrap_or("".into());
        query["filter"] = query
            .get("filter")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let payload: SearchPayload = serde_json::from_value(serde_json::json!({
            "query": query,
            "page": 0,
            "pageSize": 1000,
        }))
        .unwrap();
        let result = search_libraries(&payload, &target_libraries(&payload.query, libraries))
            .await
            .unwrap();
        let ids = result.assets.into_iter().map(|a| a.id).collect();
        (ids, result.total, result.file_total)
    }

    /// A pack with a license, a preview and a model, and a file outside it.
    const PACK_FILES: [&str; 4] = [
        "3D assets/Car Kit/License.txt",
        "3D assets/Car Kit/Preview.png",
        "3D assets/Car Kit/Models/sedan.glb",
        "Goodies/Thanks.txt",
    ];

    async fn library_with_packs(id: &str, paths: &[&str]) -> LibraryHandle {
        let library = library_of(id, paths).await;
        structure::rebuild(&library.pool, &library.root_path)
            .await
            .unwrap();
        library
    }

    #[tokio::test]
    async fn support_files_show_only_with_show_hidden_and_count_as_files() {
        let libs = [library_with_packs("lib", &PACK_FILES).await];
        let visible = vec![PACK_FILES[2].to_string(), PACK_FILES[3].to_string()];
        let all: Vec<String> = PACK_FILES.map(String::from).into();

        assert_eq!(
            counted_search(&libs, serde_json::json!({})).await,
            (visible.clone(), Some(2), Some(4))
        );
        assert_eq!(
            counted_search(
                &libs,
                serde_json::json!({ "filter": { "showHidden": true } })
            )
            .await,
            (all, Some(4), Some(4))
        );
        assert_eq!(
            counted_search(
                &libs,
                serde_json::json!({ "filter": { "showHidden": false } })
            )
            .await,
            (visible, Some(2), Some(4))
        );
    }

    #[tokio::test]
    async fn the_files_total_follows_every_other_filter() {
        let libs = [library_with_packs("lib", &PACK_FILES).await];
        let folder = serde_json::json!({ "libraryId": "lib", "path": "3D assets/Car Kit" });
        assert_eq!(
            counted_search(&libs, serde_json::json!({ "folder": folder })).await,
            (vec![PACK_FILES[2].to_string()], Some(1), Some(3))
        );
        // A search finding only support files shows no item, but counts its files.
        assert_eq!(
            counted_search(&libs, serde_json::json!({ "text": "license" })).await,
            (vec![], Some(0), Some(1))
        );
        assert_eq!(
            counted_search(
                &libs,
                serde_json::json!({ "text": "license", "filter": { "showHidden": true } })
            )
            .await,
            (vec![PACK_FILES[0].to_string()], Some(1), Some(1))
        );
    }

    #[tokio::test]
    async fn several_libraries_add_up_their_items_and_files() {
        let libs = [
            library_with_packs("a", &PACK_FILES).await,
            library_with_packs("b", &PACK_FILES[..3]).await,
        ];
        let query = serde_json::json!({ "libraryIds": ["a", "b"] });
        let (ids, total, files) = counted_search(&libs, query).await;
        assert_eq!((ids.len(), total, files), (3, Some(3), Some(7)));
        assert!(!ids.iter().any(|id| id.ends_with("License.txt")));
        let query =
            serde_json::json!({ "libraryIds": ["a", "b"], "filter": { "showHidden": true } });
        let (ids, total, files) = counted_search(&libs, query).await;
        assert_eq!((ids.len(), total, files), (7, Some(7), Some(7)));
    }

    /// A Kenney-like kit: two models in format folders, texture copies, a render and a license,
    /// next to a model on its own. Each file has its own size, so sorting by size tells them apart.
    const GROUPED_FILES: [(&str, i64); 10] = [
        ("kit/License.txt", 1),
        ("kit/Models/GLB format/sedan.glb", 30),
        ("kit/Models/FBX format/sedan.fbx", 10),
        ("kit/Models/OBJ format/sedan.obj", 20),
        ("kit/Models/GLB format/van.glb", 5),
        ("kit/Models/OBJ format/van.obj", 50),
        ("kit/Models/GLB format/Textures/colormap.png", 40),
        ("kit/Models/FBX format/Textures/colormap.png", 2),
        ("kit/Previews/sedan.png", 15),
        ("other/rock.fbx", 25),
    ];
    const SEDAN: &str = "kit/Models/GLB format/sedan.glb";
    const VAN: &str = "kit/Models/GLB format/van.glb";
    const COLORMAP: &str = "kit/Models/GLB format/Textures/colormap.png";
    const RENDER: &str = "kit/Previews/sedan.png";
    const ROCK: &str = "other/rock.fbx";

    /// A library holding `GROUPED_FILES`, each with its real extension and kind and its id the
    /// path, after its structure pass.
    async fn grouped_library() -> LibraryHandle {
        sized_library(&GROUPED_FILES).await
    }

    /// A library holding `files`, as `grouped_library` holds `GROUPED_FILES`.
    async fn sized_library(files: &[(&str, i64)]) -> LibraryHandle {
        let pool = memory_library_pool().await;
        for &(path, size) in files {
            let name = path.rsplit('/').next().unwrap();
            let ext = name.rsplit_once('.').unwrap().1;
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, preview_status)
                 VALUES (?, ?, ?, ?, ?, ?, ?, 0, 'done')",
            )
            .bind(path)
            .bind(path)
            .bind(name)
            .bind(ext)
            .bind(crate::models::AssetKind::from_extension(ext).as_str())
            .bind(size)
            .bind(size)
            .execute(&pool)
            .await
            .unwrap();
        }
        structure::rebuild(&pool, crate::structure::tests::unmounted())
            .await
            .unwrap();
        LibraryHandle {
            id: "lib".to_string(),
            name: "lib".to_string(),
            root_path: PathBuf::from("/libraries/lib"),
            pool,
        }
    }

    fn strings(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    /// `counted_search` sorted by size, smallest first.
    async fn by_size(
        libs: &[LibraryHandle],
        query: serde_json::Value,
    ) -> (Vec<String>, Option<i64>, Option<i64>) {
        let (mut ids, total, files) = counted_search(libs, query).await;
        let size = |id: &String| GROUPED_FILES.iter().find(|(p, _)| p == id).unwrap().1;
        ids.sort_by_key(size);
        (ids, total, files)
    }

    #[tokio::test]
    async fn a_groups_members_list_in_main_item_order_from_any_member() {
        let lib = grouped_library().await;
        let members = |id: &'static str| {
            let lib = &lib;
            async move {
                group_members(lib, id)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|m| (m.asset.id, m.folder, m.asset.size_bytes))
                    .collect::<Vec<_>>()
            }
        };
        let sedan = vec![
            (SEDAN.to_string(), "kit/Models/GLB format".to_string(), 30),
            (
                "kit/Models/FBX format/sedan.fbx".to_string(),
                "kit/Models/FBX format".to_string(),
                10,
            ),
            (
                "kit/Models/OBJ format/sedan.obj".to_string(),
                "kit/Models/OBJ format".to_string(),
                20,
            ),
        ];
        assert_eq!(members(SEDAN).await, sedan);
        assert_eq!(members("kit/Models/OBJ format/sedan.obj").await, sedan);
        // Copies of one format: the main item first, the rest by path.
        let colormaps: Vec<String> = members(COLORMAP)
            .await
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(
            colormaps,
            strings(&[COLORMAP, "kit/Models/FBX format/Textures/colormap.png"])
        );
        // A file on its own is listed alone.
        assert_eq!(
            members(ROCK).await,
            vec![(ROCK.to_string(), "other".to_string(), 25)]
        );
        let paths: Vec<String> = group_members(&lib, SEDAN)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.asset.file_path)
            .collect();
        assert_eq!(paths[0], format!("/libraries/lib/{SEDAN}"));
    }

    #[tokio::test]
    async fn a_groups_members_order_by_format_then_by_path_ignoring_case() {
        let ids = |lib: LibraryHandle, id: &'static str| async move {
            group_members(&lib, id)
                .await
                .unwrap()
                .into_iter()
                .map(|m| m.asset.id)
                .collect::<Vec<_>>()
        };
        // The OBJ's path sorts before the FBX's, but FBX ranks first as a format.
        let ship = sized_library(&[
            ("ship/ship.glb", 1),
            ("ship/ship.fbx", 1),
            ("ship/obj/ship.obj", 1),
        ])
        .await;
        assert_eq!(
            ids(ship, "ship/ship.glb").await,
            strings(&["ship/ship.glb", "ship/ship.fbx", "ship/obj/ship.obj"])
        );
        // Copies of one format follow their path ignoring case: `fbx/` before `OBJ/`, which
        // byte order would put first.
        let kit = sized_library(&[
            ("kit/GLB format/tex.png", 1),
            ("kit/OBJ/tex.png", 1),
            ("kit/fbx/tex.png", 1),
        ])
        .await;
        assert_eq!(
            ids(kit, "kit/OBJ/tex.png").await,
            strings(&[
                "kit/GLB format/tex.png",
                "kit/fbx/tex.png",
                "kit/OBJ/tex.png"
            ])
        );
    }

    #[tokio::test]
    async fn a_group_shows_as_its_main_item_and_its_members_never_show() {
        let libs = [grouped_library().await];
        let items = strings(&[VAN, ROCK, SEDAN, COLORMAP]);
        assert_eq!(
            by_size(&libs, serde_json::json!({})).await,
            (items.clone(), Some(4), Some(10))
        );
        // "Show hidden files" brings back the license and the sedan's render, but no member.
        // The render is of unknown size, so too small to picture the sedan, and still hidden.
        let mut with_license = items;
        with_license.insert(0, "kit/License.txt".to_string());
        with_license.insert(2, RENDER.to_string());
        assert_eq!(
            by_size(
                &libs,
                serde_json::json!({ "filter": { "showHidden": true } })
            )
            .await,
            (with_license, Some(6), Some(10))
        );
    }

    #[tokio::test]
    async fn a_group_matches_a_kind_or_an_extension_through_any_of_its_files() {
        let libs = [grouped_library().await];
        let search = |filter: serde_json::Value| {
            let libs = &libs;
            async move { by_size(libs, serde_json::json!({ "filter": filter })).await }
        };
        // The sedan through its FBX, and the rock on its own. Two files are FBX.
        assert_eq!(
            search(serde_json::json!({ "extensions": ["fbx"] })).await,
            (strings(&[ROCK, SEDAN]), Some(2), Some(2))
        );
        assert_eq!(
            search(serde_json::json!({ "extensions": ["obj"] })).await,
            (strings(&[VAN, SEDAN]), Some(2), Some(2))
        );
        assert_eq!(
            search(serde_json::json!({ "kinds": ["texture"] })).await,
            (strings(&[COLORMAP]), Some(1), Some(3))
        );
    }

    #[tokio::test]
    async fn a_format_folder_lists_its_groups_and_one_file_must_meet_every_filter() {
        let libs = [grouped_library().await];
        let search = |folder: &str, filter: serde_json::Value| {
            let query = serde_json::json!({
                "folder": { "libraryId": "lib", "path": folder },
                "filter": filter,
            });
            let libs = &libs;
            async move { by_size(libs, query).await }
        };
        let fbx = "kit/Models/FBX format";
        assert_eq!(
            search(fbx, serde_json::json!({})).await,
            (strings(&[SEDAN, COLORMAP]), Some(2), Some(2))
        );
        assert_eq!(
            search("kit/Models/OBJ format", serde_json::json!({})).await,
            (strings(&[VAN, SEDAN]), Some(2), Some(2))
        );
        // The sedan has a GLB and a file in `FBX format/`, but no GLB there.
        assert_eq!(
            search(fbx, serde_json::json!({ "extensions": ["glb"] })).await,
            (vec![], Some(0), Some(0))
        );
        assert_eq!(
            search(fbx, serde_json::json!({ "extensions": ["fbx"] })).await,
            (strings(&[SEDAN]), Some(1), Some(1))
        );
        assert_eq!(
            search(fbx, serde_json::json!({ "namePrefix": "colormap" })).await,
            (strings(&[COLORMAP]), Some(1), Some(1))
        );
        assert_eq!(
            search(fbx, serde_json::json!({ "namePrefix": "van" })).await,
            (vec![], Some(0), Some(0))
        );
        // A folder no group splits: its items are those below it, with every file it holds.
        assert_eq!(
            search("kit/Models", serde_json::json!({})).await,
            (strings(&[VAN, SEDAN, COLORMAP]), Some(3), Some(7))
        );
        assert_eq!(
            search("kit/Models", serde_json::json!({ "extensions": ["fbx"] })).await,
            (strings(&[SEDAN]), Some(1), Some(1))
        );
        // The smart filter's directory reads members the same way.
        assert_eq!(
            by_size(&libs, serde_json::json!({ "filter": { "directory": fbx } })).await,
            (strings(&[SEDAN, COLORMAP]), Some(2), Some(2))
        );
    }

    #[tokio::test]
    async fn search_text_finds_a_group_through_a_word_only_a_members_folder_has() {
        let libs = [grouped_library().await];
        // "fbx" is in the sedan's and the colormap's FBX copies' paths, and the rock's name.
        assert_eq!(
            by_size(&libs, serde_json::json!({ "text": "fbx" })).await,
            (strings(&[ROCK, SEDAN, COLORMAP]), Some(3), Some(3))
        );
        // No GLB has "fbx" in its path.
        assert_eq!(
            by_size(
                &libs,
                serde_json::json!({ "text": "fbx", "filter": { "extensions": ["glb"] } })
            )
            .await,
            (vec![], Some(0), Some(0))
        );
        assert_eq!(
            by_size(&libs, serde_json::json!({ "text": "sedan" })).await,
            (strings(&[SEDAN]), Some(1), Some(4))
        );
    }

    #[tokio::test]
    async fn metadata_filters_and_sort_keys_read_the_main_item() {
        let libs = [grouped_library().await];
        let pool = &libs[0].pool;
        for (id, rating) in [(SEDAN, 4), ("kit/Models/OBJ format/van.obj", 5)] {
            sqlx::query("UPDATE assets SET rating = ? WHERE id = ?")
                .bind(rating)
                .bind(id)
                .execute(pool)
                .await
                .unwrap();
        }
        let (rated, total, _) = counted_search(
            &libs,
            serde_json::json!({ "filter": { "rating": { "min": 4, "max": 5 } } }),
        )
        .await;
        assert_eq!((rated, total), (strings(&[SEDAN]), Some(1)));

        // By the main items' sizes: the van's GLB is the smallest file of all but the license
        // and the colormap's FBX copy, and the sedan's GLB is larger than its FBX and OBJ.
        for (field, order, expected) in [
            ("size", "asc", [VAN, ROCK, SEDAN, COLORMAP]),
            ("size", "desc", [COLORMAP, SEDAN, ROCK, VAN]),
            ("name", "asc", [COLORMAP, ROCK, SEDAN, VAN]),
        ] {
            assert_eq!(
                folder_search(&libs, serde_json::json!({}), field, order).await,
                strings(&expected),
                "{field} {order}"
            );
        }
    }

    #[tokio::test]
    async fn every_folders_item_count_is_the_grid_total_for_that_folder() {
        let libs = [grouped_library().await];
        let folders: Vec<(String, i64, i64)> =
            sqlx::query_as("SELECT path, item_count, hidden_count FROM folders ORDER BY path")
                .fetch_all(&libs[0].pool)
                .await
                .unwrap();
        assert_eq!(folders.len(), 10);
        for (path, items, hidden) in folders {
            for (show_hidden, expected) in [(false, items), (true, items + hidden)] {
                let query = serde_json::json!({
                    "folder": { "libraryId": "lib", "path": path },
                    "filter": { "showHidden": show_hidden },
                });
                let (_, total, _) = counted_search(&libs, query).await;
                assert_eq!(
                    total,
                    Some(expected),
                    "{path:?}, hidden files {show_hidden}"
                );
            }
        }
    }

    /// A search query from `query`, with an empty text and filter unless it sets them.
    fn query_of(query: serde_json::Value) -> SearchQuery {
        let mut query = query;
        query["sort"] = serde_json::json!({ "field": "name", "order": "asc" });
        query["text"] = query.get("text").cloned().unwrap_or("".into());
        query["filter"] = query
            .get("filter")
            .cloned()
            .unwrap_or(serde_json::json!({}));
        serde_json::from_value(query).unwrap()
    }

    /// The live folder counts of `query` for the children of `parents` in library `lib`.
    async fn live_folder_counts(
        library: &LibraryHandle,
        query: serde_json::Value,
        parents: &[&str],
    ) -> HashMap<String, HashMap<String, i64>> {
        let parents: Vec<String> = parents.iter().map(|p| p.to_string()).collect();
        folder_counts(&library.pool, &query_of(query), &parents)
            .await
            .unwrap()
    }

    /// The live kind and extension counts of `query` in `libraries`.
    async fn live_facet_counts(
        libraries: &[LibraryHandle],
        query: serde_json::Value,
    ) -> FacetCounts {
        let query = query_of(query);
        facet_counts(&query, &target_libraries(&query, libraries))
            .await
            .unwrap()
    }

    fn count_map(pairs: &[(&str, i64)]) -> HashMap<String, i64> {
        pairs.iter().map(|(k, n)| (k.to_string(), *n)).collect()
    }

    #[tokio::test]
    async fn every_live_count_is_the_grid_total_for_that_folder_kind_or_extension() {
        let libs = [grouped_library().await];
        let pool = &libs[0].pool;
        let folders: Vec<String> = sqlx::query_scalar("SELECT path FROM folders")
            .fetch_all(pool)
            .await
            .unwrap();
        let parents: Vec<&str> = folders.iter().map(String::as_str).collect();
        let kinds: Vec<String> = sqlx::query_scalar("SELECT DISTINCT kind FROM assets")
            .fetch_all(pool)
            .await
            .unwrap();
        let extensions: Vec<String> = sqlx::query_scalar("SELECT DISTINCT extension FROM assets")
            .fetch_all(pool)
            .await
            .unwrap();
        let folder = |path: &str| serde_json::json!({ "libraryId": "lib", "path": path });
        let queries = [
            serde_json::json!({}),
            serde_json::json!({ "text": "sedan" }),
            serde_json::json!({ "text": "fbx" }),
            serde_json::json!({ "text": "colormap", "filter": { "showHidden": true } }),
            serde_json::json!({ "text": "license", "filter": { "showHidden": true } }),
            serde_json::json!({ "filter": { "extensions": ["obj"] } }),
            serde_json::json!({ "filter": { "kinds": ["texture"], "namePrefix": "c" } }),
            serde_json::json!({ "filter": { "directory": "kit/Models/FBX format" } }),
            serde_json::json!({ "folder": folder("kit/Models/FBX format") }),
            serde_json::json!({ "folder": folder("kit"), "text": "sedan" }),
            serde_json::json!({ "filter": { "extensions": ["png", "obj"] } }),
            serde_json::json!({ "text": "fbx", "filter": { "extensions": ["glb"] } }),
            serde_json::json!({ "folder": folder("kit/Models"), "filter": { "extensions": ["fbx"] } }),
        ];
        let mut nonzero = 0;
        for query in queries {
            let total = |patch: serde_json::Value| {
                let mut q = query.clone();
                for (key, value) in patch.as_object().unwrap() {
                    if key == "filter" {
                        let mut filter = q.get("filter").cloned().unwrap_or(serde_json::json!({}));
                        for (k, v) in value.as_object().unwrap() {
                            filter[k] = v.clone();
                        }
                        q["filter"] = filter;
                    } else {
                        q[key] = value.clone();
                    }
                }
                let libs = &libs;
                async move { counted_search(libs, q).await.1.unwrap() }
            };

            let counts = live_folder_counts(&libs[0], query.clone(), &parents).await;
            for child in folders.iter().filter(|p| !p.is_empty()) {
                let parent = child.rsplit_once('/').map_or("", |(p, _)| p);
                let count = counts[parent].get(child).copied().unwrap_or(0);
                let expected = total(serde_json::json!({ "folder": folder(child) })).await;
                assert_eq!(count, expected, "{query}: folder {child:?}");
                nonzero += (count > 0) as usize;
            }
            let facets = live_facet_counts(&libs, query.clone()).await;
            for kind in &kinds {
                let count = facets.kinds.get(kind).copied().unwrap_or(0);
                let expected = total(serde_json::json!({ "filter": { "kinds": [kind] } })).await;
                assert_eq!(count, expected, "{query}: kind {kind}");
            }
            // A format pill adds its extension to the active ones; `txt` only the license has.
            let active: Vec<String> =
                serde_json::from_value(query["filter"]["extensions"].clone()).unwrap_or_default();
            for ext in extensions
                .iter()
                .map(String::as_str)
                .chain(["txt", "nothing"])
            {
                let count = facets.extension(ext);
                let mut picked = active.clone();
                if !picked.iter().any(|e| e == ext) {
                    picked.push(ext.to_string());
                }
                let expected =
                    total(serde_json::json!({ "filter": { "extensions": picked } })).await;
                assert_eq!(count, expected, "{query}: extension {ext}");
            }
        }
        assert!(nonzero > 20, "only {nonzero} folder counts were above 0");
    }

    #[tokio::test]
    async fn no_search_or_live_count_matches_the_words_of_a_preview_texture_report() {
        let ship = "ships/Models/ship.fbx";
        let rock = "textures/rock.png";
        let libs = [sized_library(&[(ship, 1), (rock, 1)]).await];
        let lib = &libs[0];
        // Completed as the worker reports an FBX it couldn't fully texture (plan C7)
        let result = serde_json::json!({
            "ok": true,
            "preview_version": 2,
            "preview_textures": {
                "materials": 2, "textured": 1, "via": {"name": 1}, "missing": ["T_Rock_BC"],
                "alpha": {"cutout": 0, "ignored": 0, "none": 1},
            },
        });
        let model = lib.root_path.join(".opendam/thumbs/ship_preview.glb");
        crate::preview::complete_job(
            &lib.pool,
            &lib.root_path,
            "j1",
            ship,
            Some(".opendam/thumbs/ship_preview.glb".to_string()),
            Some(model.to_string_lossy().to_string()),
            &crate::preview::EngineReport::of_result(&result),
        )
        .await;
        let report: String = sqlx::query_scalar("SELECT preview_textures FROM assets WHERE id = ?")
            .bind(ship)
            .fetch_one(&lib.pool)
            .await
            .unwrap();
        assert!(report.contains("T_Rock_BC"));
        assert_eq!(
            counted_search(&libs, serde_json::json!({ "text": "ship" }))
                .await
                .0,
            [ship]
        );

        for (text, found) in [
            ("rock", &[rock][..]),
            ("none", &[]),
            ("name", &[]),
            ("textured", &[]),
        ] {
            let query = serde_json::json!({ "text": text });
            let (ids, total, _) = counted_search(&libs, query.clone()).await;
            assert_eq!(ids, found, "{text}");
            assert_eq!(total, Some(found.len() as i64), "{text}");
            let counts = live_folder_counts(lib, query.clone(), &["", "ships"]).await;
            assert!(!counts[""].contains_key("ships"), "{text}: {counts:?}");
            assert!(counts["ships"].is_empty(), "{text}: {counts:?}");
            let facets = live_facet_counts(&libs, query).await;
            assert_eq!(facets.extension("fbx"), 0, "{text}");
            assert!(!facets.kinds.contains_key("3d_model"), "{text}");
        }
    }

    #[tokio::test]
    async fn a_kits_format_folders_each_count_its_models_and_their_parent_counts_them_once() {
        let library = grouped_library().await;
        let counts = live_folder_counts(
            &library,
            serde_json::json!({ "filter": { "kinds": ["3d_model"] } }),
            &["kit/Models", "kit", ""],
        )
        .await;
        assert_eq!(
            counts["kit/Models"],
            count_map(&[
                ("kit/Models/GLB format", 2),
                ("kit/Models/FBX format", 1),
                ("kit/Models/OBJ format", 2)
            ])
        );
        // Sibling counts add up to more than their parent's: each model counts once in `Models/`.
        assert_eq!(counts["kit"], count_map(&[("kit/Models", 2)]));
        assert_eq!(counts[""], count_map(&[("kit", 2), ("other", 1)]));
    }

    #[tokio::test]
    async fn search_text_only_a_members_folder_matches_counts_the_group_only_under_that_folder() {
        let libs = [grouped_library().await];
        let query = serde_json::json!({ "text": "fbx" });
        let counts = live_folder_counts(&libs[0], query.clone(), &["kit/Models", ""]).await;
        // The sedan and the colormap through their FBX copies, and no GLB or OBJ folder.
        assert_eq!(
            counts["kit/Models"],
            count_map(&[("kit/Models/FBX format", 2)])
        );
        assert_eq!(counts[""], count_map(&[("kit", 2), ("other", 1)]));
        let facets = live_facet_counts(&libs, query).await;
        assert_eq!(
            facets,
            FacetCounts {
                kinds: count_map(&[("3d_model", 2), ("texture", 1)]),
                extensions: count_map(&[("fbx", 2), ("png", 1)]),
                extension_base: 0,
            }
        );
    }

    #[tokio::test]
    async fn a_format_count_adds_its_extension_to_the_active_ones() {
        let libs = [grouped_library().await];
        // The only PNG item is the colormap, the sedan's render being hidden; each model adds its
        // own formats to it.
        let query = serde_json::json!({ "filter": { "extensions": ["png"] } });
        let facets = live_facet_counts(&libs, query).await;
        assert_eq!(facets.extension_base, 1);
        assert_eq!(
            facets.extensions,
            count_map(&[("fbx", 3), ("glb", 3), ("obj", 3)])
        );
        let (_, total, _) = counted_search(
            &libs,
            serde_json::json!({ "filter": { "extensions": ["png", "fbx"] } }),
        )
        .await;
        assert_eq!(total, Some(3));
    }

    #[tokio::test]
    async fn folder_counts_leave_out_the_collection_selecting_a_folder_replaces() {
        let libs = [grouped_library().await];
        let pool = &libs[0].pool;
        insert_collection(pool, "garage", "garage", None).await;
        sqlx::query("INSERT INTO collection_assets (collection_id, asset_id) VALUES ('garage', ?)")
            .bind(SEDAN)
            .execute(pool)
            .await
            .unwrap();
        let query = serde_json::json!({ "text": "fbx", "collectionId": "garage" });
        let counts = live_folder_counts(&libs[0], query.clone(), &[""]).await;
        assert_eq!(counts[""], count_map(&[("kit", 2), ("other", 1)]));
        let facets = live_facet_counts(&libs, query).await;
        assert_eq!(facets.extensions, count_map(&[("fbx", 1)]));
    }

    #[tokio::test]
    async fn facet_counts_add_up_every_library_searched() {
        let libs = [grouped_library().await, {
            let mut other = grouped_library().await;
            other.id = "other".to_string();
            other
        }];
        let facets = live_facet_counts(&libs, serde_json::json!({ "text": "fbx" })).await;
        assert_eq!(facets.extensions, count_map(&[("fbx", 4), ("png", 2)]));
        let one = live_facet_counts(
            &libs,
            serde_json::json!({ "text": "fbx", "libraryId": "other" }),
        )
        .await;
        assert_eq!(one.extensions, count_map(&[("fbx", 2), ("png", 1)]));
        // With FBX active, the colormap's PNG copy adds one item in each library.
        let fbx = live_facet_counts(
            &libs,
            serde_json::json!({ "text": "fbx", "filter": { "extensions": ["fbx"] } }),
        )
        .await;
        assert_eq!(
            (fbx.extension_base, fbx.extensions),
            (4, count_map(&[("png", 6)]))
        );
    }

    #[tokio::test]
    async fn a_main_item_lists_its_groups_formats_and_a_file_on_its_own_none() {
        let libs = [grouped_library().await];
        let payload: SearchPayload = serde_json::from_value(serde_json::json!({
            "query": { "text": "", "filter": {}, "sort": { "field": "size", "order": "asc" } },
            "page": 0,
            "pageSize": 100,
        }))
        .unwrap();
        let result = search_libraries(&payload, &target_libraries(&payload.query, &libs))
            .await
            .unwrap();
        let formats: Vec<(String, Option<Vec<String>>)> = result
            .assets
            .into_iter()
            .map(|a| (a.file_name, a.group_formats))
            .collect();
        let some = |f: &[&str]| Some(strings(f));
        assert_eq!(
            formats,
            [
                ("van.glb".to_string(), some(&["glb", "obj"])),
                ("rock.fbx".to_string(), None),
                ("sedan.glb".to_string(), some(&["glb", "fbx", "obj"])),
                ("colormap.png".to_string(), some(&["png", "png"])),
            ]
        );
    }

    #[tokio::test]
    async fn an_items_render_is_its_bundled_preview_when_it_fits_else_its_fallback_render() {
        let libs = [grouped_library().await];
        let pool = &libs[0].pool;
        let pictures = || async {
            let payload: SearchPayload = serde_json::from_value(serde_json::json!({
                "query": { "text": "", "filter": {}, "sort": { "field": "size", "order": "asc" } },
                "page": 0,
                "pageSize": 100,
            }))
            .unwrap();
            search_libraries(&payload, &target_libraries(&payload.query, &libs))
                .await
                .unwrap()
                .assets
                .into_iter()
                .filter(|a| a.bundled_preview.is_some() || a.fallback_render.is_some())
                .map(|a| (a.id, a.bundled_preview, a.fallback_render))
                .collect::<Vec<_>>()
        };
        let set = |sql: &'static str| async move {
            sqlx::query(sql).bind(RENDER).execute(pool).await.unwrap();
        };
        let render = Some("/libraries/lib/kit/Previews/sedan.png".to_string());
        let thumb = Some("/libraries/lib/.opendam/thumbs/r_thumb.png".to_string());
        let shows = |bundled: &Option<String>, fallback: &Option<String>| {
            vec![(SEDAN.to_string(), bundled.clone(), fallback.clone())]
        };

        // Unmounted, the render's size is unknown, so it doesn't fit, and it has no thumbnail, as
        // one of 512 px or less never does.
        assert_eq!(pictures().await, shows(&None, &render));
        set("UPDATE assets SET render_width = 512, render_height = 511 WHERE id = ?").await;
        assert_eq!(pictures().await, shows(&None, &render));
        set("UPDATE assets SET thumbnail_path = '.opendam/thumbs/r_thumb.png' WHERE id = ?").await;
        assert_eq!(pictures().await, shows(&None, &thumb));

        // Both sides at least 512 px: it fits
        set("UPDATE assets SET render_height = 512 WHERE id = ?").await;
        assert_eq!(pictures().await, shows(&thumb, &None));
        set("UPDATE assets SET thumbnail_path = NULL WHERE id = ?").await;
        assert_eq!(pictures().await, shows(&render, &None));
    }

    #[tokio::test]
    async fn a_search_over_several_libraries_shows_each_groups_main_item_once() {
        let mut second = grouped_library().await;
        second.id = "second".to_string();
        let libs = [grouped_library().await, second];
        let query = serde_json::json!({ "filter": { "extensions": ["fbx"] } });
        let (ids, total, files) = counted_search(&libs, query).await;
        assert_eq!(
            (ids, total, files),
            (strings(&[ROCK, ROCK, SEDAN, SEDAN]), Some(4), Some(4))
        );
    }

    #[tokio::test]
    async fn a_main_item_with_no_thumbnail_shows_a_members() {
        let pool = memory_library_pool().await;
        for (path, thumb) in [
            ("kit/Blend/rock.blend", None),
            ("kit/OBJ/rock.obj", Some(".opendam/thumbs/rock_obj.png")),
        ] {
            let name = path.rsplit('/').next().unwrap();
            let ext = name.rsplit_once('.').unwrap().1;
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, thumbnail_path)
                 VALUES (?, ?, ?, ?, '3d_model', 1, 0, 0, ?)",
            )
            .bind(path)
            .bind(path)
            .bind(name)
            .bind(ext)
            .bind(thumb)
            .execute(&pool)
            .await
            .unwrap();
        }
        structure::rebuild(&pool, crate::structure::tests::unmounted())
            .await
            .unwrap();
        let libs = [LibraryHandle {
            id: "lib".to_string(),
            name: "lib".to_string(),
            root_path: PathBuf::from("/libraries/lib"),
            pool,
        }];
        let payload: SearchPayload = serde_json::from_value(serde_json::json!({
            "query": { "text": "", "filter": {}, "sort": { "field": "name", "order": "asc" } },
            "page": 0,
            "pageSize": 10,
        }))
        .unwrap();
        let result = search_libraries(&payload, &target_libraries(&payload.query, &libs))
            .await
            .unwrap();
        let shown: Vec<(String, Option<String>)> = result
            .assets
            .into_iter()
            .map(|a| (a.file_name, a.thumbnail_path))
            .collect();
        assert_eq!(
            shown,
            [(
                "rock.blend".to_string(),
                Some("/libraries/lib/.opendam/thumbs/rock_obj.png".to_string())
            )]
        );
    }

    #[test]
    fn a_groups_formats_list_the_main_items_first_then_by_rank() {
        let formats = strings(&["obj", "fbx", "dae", "glb", "blend", "stl"]);
        assert_eq!(
            main_first("fbx", formats),
            strings(&["fbx", "glb", "blend", "obj", "dae", "stl"])
        );
    }

    const KIT_FILES: [&str; 9] = [
        "3D assets/Platformer Kit/License.txt",
        "3D assets/Platformer Kit/Models/GLB format/block.glb",
        "3D assets/Platformer Kit/Models/OBJ format/block.obj",
        "3D assets/Platformer Kit 2/crate.glb",
        "3D assets/Platformer Kit-Extra/coin.glb",
        "3D assets/Platformer Kit.zip",
        "3D assets/Platformer Kit0/flag.glb",
        "3D assets/Platformer KitZ/tree.glb",
        "2D assets/Platformer Kit/tile.png",
    ];

    #[tokio::test]
    async fn a_folder_matches_the_files_below_it_and_no_sibling_sharing_its_name() {
        let libs = [library_of("lib", &KIT_FILES).await];
        let kit = [
            "3D assets/Platformer Kit/License.txt",
            "3D assets/Platformer Kit/Models/GLB format/block.glb",
            "3D assets/Platformer Kit/Models/OBJ format/block.obj",
        ];
        for path in [
            "3D assets/Platformer Kit",
            "3D assets/Platformer Kit/",
            "/3D assets/Platformer Kit/",
        ] {
            let mut found = folder_search(
                &libs,
                serde_json::json!({ "folder": { "libraryId": "lib", "path": path } }),
                "date",
                "asc",
            )
            .await;
            found.sort();
            assert_eq!(found, kit, "folder {path:?}");
        }
        assert_eq!(
            folder_search(
                &libs,
                serde_json::json!({ "folder": { "libraryId": "lib", "path": "3D assets/Platformer Kit/Models/OBJ format" } }),
                "name",
                "asc",
            )
            .await,
            ["3D assets/Platformer Kit/Models/OBJ format/block.obj"]
        );
    }

    #[tokio::test]
    async fn a_folder_named_with_edge_spaces_matches_only_its_own_files() {
        let libs = [library_of(
            "lib",
            &["Textures /a.png", "Textures/b.png", " Models/c.png"],
        )
        .await];
        for (path, expected) in [
            ("Textures ", "Textures /a.png"),
            ("Textures", "Textures/b.png"),
            (" Models", " Models/c.png"),
        ] {
            let found = folder_search(
                &libs,
                serde_json::json!({ "folder": { "libraryId": "lib", "path": path } }),
                "name",
                "asc",
            )
            .await;
            assert_eq!(found, [expected], "folder {path:?}");
        }
        // The Smart Filter's hand-typed folder still drops its padding.
        assert_eq!(
            folder_search(
                &libs,
                serde_json::json!({ "filter": { "directory": " Textures/ " } }),
                "name",
                "asc",
            )
            .await,
            ["Textures/b.png"]
        );
    }

    #[tokio::test]
    async fn the_directory_filter_is_the_same_folder_range() {
        let libs = [library_of("lib", &KIT_FILES).await];
        let mut found = folder_search(
            &libs,
            serde_json::json!({ "filter": { "directory": "3D assets/Platformer Kit" } }),
            "date",
            "asc",
        )
        .await;
        found.sort();
        assert_eq!(
            found,
            [
                "3D assets/Platformer Kit/License.txt",
                "3D assets/Platformer Kit/Models/GLB format/block.glb",
                "3D assets/Platformer Kit/Models/OBJ format/block.obj",
            ]
        );
        // A name inside the path no longer matches as a substring.
        assert!(folder_search(
            &libs,
            serde_json::json!({ "filter": { "directory": "Platformer Kit" } }),
            "date",
            "asc",
        )
        .await
        .is_empty());
    }

    #[tokio::test]
    async fn a_folder_keeps_every_sort_order_and_its_rowid_tie_break() {
        let libs = [library_of("lib", &KIT_FILES).await];
        let folder = "3D assets/Platformer Kit/";
        for field in ["name", "date", "size", "rating", "kind", "favorite"] {
            for order in ["asc", "desc"] {
                let all = folder_search(&libs, serde_json::json!({}), field, order).await;
                let below: Vec<String> = all
                    .into_iter()
                    .filter(|id| id.starts_with(folder))
                    .collect();
                let found = folder_search(
                    &libs,
                    serde_json::json!({ "folder": { "libraryId": "lib", "path": folder } }),
                    field,
                    order,
                )
                .await;
                assert_eq!(found, below, "sorted by {field} {order}");
            }
        }
    }

    #[tokio::test]
    async fn the_library_root_matches_its_whole_library_and_no_other() {
        let libs = [
            library_of("a", &["3D assets/a.glb", "b.glb"]).await,
            library_of("b", &["3D assets/c.glb", "d.glb"]).await,
        ];
        for root in ["", "/"] {
            let mut found = folder_search(
                &libs,
                serde_json::json!({ "folder": { "libraryId": "b", "path": root }, "libraryIds": ["a", "b"] }),
                "name",
                "asc",
            )
            .await;
            found.sort();
            assert_eq!(found, ["3D assets/c.glb", "d.glb"], "root {root:?}");
        }
        assert!(folder_search(
            &libs,
            serde_json::json!({ "folder": { "libraryId": "gone", "path": "" } }),
            "name",
            "asc",
        )
        .await
        .is_empty());
    }

    #[tokio::test]
    async fn a_directory_across_libraries_merges_their_matches() {
        let libs = [
            library_of("a", &["3D assets/b.glb", "3D assets/d.glb", "c.glb"]).await,
            library_of("b", &["3D assets/a.glb", "3D assets/c.glb", "b.glb"]).await,
        ];
        assert_eq!(
            folder_search(
                &libs,
                serde_json::json!({ "filter": { "directory": "3D assets" } }),
                "name",
                "asc",
            )
            .await,
            [
                "3D assets/a.glb",
                "3D assets/b.glb",
                "3D assets/c.glb",
                "3D assets/d.glb"
            ]
        );
    }

    /// The golden snapshot's files, for the license headers the structure pass reads:
    /// `OPENDAM_GOLDEN_ROOT`, or the snapshot's usual place.
    pub(crate) fn golden_root() -> PathBuf {
        std::env::var_os("OPENDAM_GOLDEN_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").expect("HOME is set"))
                    .join(".cache/opendam-bench/kenney-golden")
            })
    }

    /// Checks the folder range on a copy of the golden snapshot's library DB, named by
    /// `OPENDAM_GOLDEN_DB` (opening it runs the schema, so never point it at a live library):
    /// `cargo nextest run --run-ignored only golden_snapshot`.
    #[tokio::test]
    #[ignore = "needs OPENDAM_GOLDEN_DB, a copy of the golden snapshot's library.sqlite"]
    async fn a_folder_on_the_golden_snapshot_matches_a_sql_count() {
        let db = std::env::var("OPENDAM_GOLDEN_DB").expect("OPENDAM_GOLDEN_DB is set");
        let pool = crate::db::connect_library_sqlite(Path::new(&db))
            .await
            .unwrap();
        let library = LibraryHandle {
            id: "golden".to_string(),
            name: "golden".to_string(),
            root_path: PathBuf::from("/golden"),
            pool: pool.clone(),
        };
        async fn count(pool: &Pool<Sqlite>, sql: &str, bind: &str) -> i64 {
            sqlx::query_scalar(sql)
                .bind(bind)
                .fetch_one(pool)
                .await
                .unwrap()
        }
        let libs = [library];
        // The files below the folder, hidden support files included, as the SQL counts them.
        let total = |path: &str| {
            let payload: SearchPayload = serde_json::from_value(serde_json::json!({
                "query": {
                    "text": "",
                    "filter": {},
                    "sort": { "field": "name", "order": "asc" },
                    "folder": { "libraryId": "golden", "path": path },
                },
                "page": 0,
                "pageSize": 100,
            }))
            .unwrap();
            let libs = &libs;
            async move {
                let targets = target_libraries(&payload.query, libs);
                search_libraries(&payload, &targets)
                    .await
                    .unwrap()
                    .file_total
                    .unwrap()
            }
        };

        // `Nature Kit (Classic)` shares its name's start with `Nature Kit`.
        for folder in ["3D assets/Platformer Kit", "3D assets/Nature Kit"] {
            let expected = count(
                &pool,
                "SELECT COUNT(*) FROM assets WHERE substr(file_path, 1, length(?1) + 1) = ?1 || '/'",
                folder,
            )
            .await;
            let siblings = count(
                &pool,
                "SELECT COUNT(*) FROM assets
                 WHERE substr(file_path, 1, length(?1)) = ?1
                   AND substr(file_path, 1, length(?1) + 1) <> ?1 || '/'",
                folder,
            )
            .await;
            let found = total(folder).await;
            println!(
                "{folder}: search files total {found}, SQL count {expected}, files in prefix-sharing siblings {siblings}"
            );
            assert!(expected > 0);
            assert_eq!(found, expected);
        }
        let root = total("").await;
        let all = count(&pool, "SELECT COUNT(*) FROM assets WHERE ?1 = ''", "").await;
        println!("library root: search files total {root}, SQL count {all}");
        assert_eq!(root, all);
    }

    /// The structure pass's targets (A1) and the tree's node load time (A5) on a copy of the
    /// golden snapshot's library DB, named by `OPENDAM_GOLDEN_DB` as above. It writes to the copy:
    /// it clears `folders` and the support roles for a first pass, then deletes and renames one
    /// asset. Run it with
    /// `cargo nextest run --run-ignored only golden_snapshot --no-capture`.
    #[tokio::test]
    #[ignore = "needs OPENDAM_GOLDEN_DB, a copy of the golden snapshot's library.sqlite"]
    async fn the_folder_tree_of_the_golden_snapshot_meets_its_time_targets() {
        use crate::commands::folder::child_folders;
        use std::time::{Duration, Instant};
        let file = std::env::var("OPENDAM_GOLDEN_DB").expect("OPENDAM_GOLDEN_DB is set");
        let pool = crate::db::connect_library_sqlite(Path::new(&file))
            .await
            .unwrap();
        // A root that doesn't exist, so the rename and the delete touch no file.
        let library = LibraryHandle {
            id: "golden".to_string(),
            name: "golden".to_string(),
            root_path: PathBuf::from("/golden-snapshot-not-mounted"),
            pool: pool.clone(),
        };
        let db = memory_database(vec![library.clone()]).await;
        let timed = |what: &str, took: Duration, limit: Duration| {
            println!("{what}: {took:?} (target under {limit:?})");
            assert!(took < limit, "{what} took {took:?}");
        };

        sqlx::query("DELETE FROM folders; UPDATE assets SET support_role = NULL;")
            .execute(&pool)
            .await
            .unwrap();
        let started = Instant::now();
        let written = crate::structure::rebuild(&pool, &golden_root())
            .await
            .unwrap();
        timed(
            &format!("first pass, {written} rows written"),
            started.elapsed(),
            Duration::from_secs(10),
        );
        let started = Instant::now();
        let written = crate::structure::rebuild(&pool, &golden_root())
            .await
            .unwrap();
        timed(
            &format!("rerun with no changes, {written} rows written"),
            started.elapsed(),
            Duration::from_secs(2),
        );
        assert_eq!(written, 0);

        // Every node's children load within 50 ms, and each count equals its folder's search total.
        let mut slowest = (Duration::ZERO, String::new());
        let mut nodes = 0;
        let mut queue = vec![String::new()];
        while let Some(parent) = queue.pop() {
            let started = Instant::now();
            let children = child_folders(&pool, &library.root_path, &parent)
                .await
                .unwrap();
            let took = started.elapsed();
            if took > slowest.0 {
                slowest = (took, parent);
            }
            nodes += 1;
            queue.extend(children.into_iter().map(|c| c.path));
        }
        timed(
            &format!("slowest of {nodes} node loads ({:?})", slowest.1),
            slowest.0,
            Duration::from_millis(50),
        );
        let top = child_folders(&pool, &library.root_path, "").await.unwrap();
        println!(
            "top-level folders: {}",
            top.iter()
                .map(|f| format!("{} ({})", f.name, f.item_count))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let kit = child_folders(&pool, &library.root_path, "3D assets")
            .await
            .unwrap()
            .into_iter()
            .find(|f| f.name == "Platformer Kit")
            .expect("a 3D assets/Platformer Kit node");
        let payload: SearchPayload = serde_json::from_value(serde_json::json!({
            "query": {
                "text": "",
                "filter": {},
                "sort": { "field": "name", "order": "asc" },
                "folder": { "libraryId": "golden", "path": kit.path },
            },
            "page": 0,
            "pageSize": 10_000,
        }))
        .unwrap();
        let libs = [library.clone()];
        let found = search_libraries(&payload, &target_libraries(&payload.query, &libs))
            .await
            .unwrap();
        let prefix = format!("/golden-snapshot-not-mounted/{}/", kit.path);
        println!(
            "{}: node count {}, search total {:?}",
            kit.path, kit.item_count, found.total
        );
        assert_eq!(found.total, Some(kit.item_count));
        assert!(found
            .assets
            .iter()
            .all(|a| a.file_path.starts_with(&prefix)));

        // A rename and a delete each rerun the pass before they return. The file is an item on
        // its own, so the delete takes one from the kit's count.
        let (id, path): (String, String) = sqlx::query_as(
            "SELECT id, file_path FROM assets WHERE file_path LIKE '3D assets/Platformer Kit/%'
               AND support_role IS NULL AND group_id IS NULL
             ORDER BY file_path LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let started = Instant::now();
        rename_asset_with(&db, &id, "renamed-by-golden-test", fs_rename)
            .await
            .unwrap();
        timed(
            &format!("rename of {path}, with its pass"),
            started.elapsed(),
            Duration::from_secs(2),
        );
        let started = Instant::now();
        delete_assets_with(&db, &[id], false, |_| Ok::<(), &str>(()))
            .await
            .unwrap();
        timed(
            "delete with its pass",
            started.elapsed(),
            Duration::from_secs(2),
        );
        let after = child_folders(&pool, &library.root_path, "3D assets")
            .await
            .unwrap()
            .into_iter()
            .find(|f| f.name == "Platformer Kit")
            .unwrap();
        println!(
            "after the delete, {} counts {}",
            after.path, after.item_count
        );
        assert_eq!(after.item_count, kit.item_count - 1);
        assert_eq!(
            crate::structure::rebuild(&pool, &golden_root())
                .await
                .unwrap(),
            0
        );
    }

    /// The packs (A2) and the hidden support files on a copy of the golden snapshot's library DB,
    /// named by `OPENDAM_GOLDEN_DB` as above: `cargo nextest run --run-ignored only
    /// golden_snapshot --no-capture`.
    #[tokio::test]
    #[ignore = "needs OPENDAM_GOLDEN_DB, a copy of the golden snapshot's library.sqlite"]
    async fn the_packs_of_the_golden_snapshot_hide_their_support_files() {
        let file = std::env::var("OPENDAM_GOLDEN_DB").expect("OPENDAM_GOLDEN_DB is set");
        let pool = crate::db::connect_library_sqlite(Path::new(&file))
            .await
            .unwrap();
        let packs: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT path, item_count, hidden_count FROM folders WHERE is_pack ORDER BY path",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let all_hidden: Vec<&str> = packs
            .iter()
            .filter(|(_, items, _)| *items == 0)
            .map(|(path, _, _)| path.as_str())
            .collect();
        println!(
            "{} packs, {} with every file hidden: {}",
            packs.len(),
            all_hidden.len(),
            all_hidden.join(", ")
        );
        assert_eq!(packs.len(), 254);
        assert_eq!(all_hidden.len(), 24);
        // Every pack is `<category>/<pack>`, so no category folder is one.
        let deeper: Vec<&str> = packs
            .iter()
            .map(|(path, _, _)| path.as_str())
            .filter(|path| path.matches('/').count() != 1)
            .collect();
        assert_eq!(deeper, [] as [&str; 0]);
        for not_pack in ["Goodies/Wallpapers", "Goodies", "Archive", "Early access"] {
            assert!(
                !packs.iter().any(|(path, _, _)| path == not_pack),
                "{not_pack} is a pack"
            );
        }
        let roles: Vec<(String, i64)> = sqlx::query_as(
            "SELECT support_role, COUNT(*) FROM assets WHERE support_role IS NOT NULL
             GROUP BY support_role ORDER BY support_role",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        println!("support files: {roles:?}");

        // The grid hides `License.txt` and `Preview.png`, and "Show hidden files" brings them back.
        let library = LibraryHandle {
            id: "golden".to_string(),
            name: "golden".to_string(),
            root_path: PathBuf::from("/golden"),
            pool: pool.clone(),
        };
        let libs = [library];
        let search = |text: &str, show_hidden: bool| {
            let payload: SearchPayload = serde_json::from_value(serde_json::json!({
                "query": {
                    "text": text,
                    "filter": { "showHidden": show_hidden },
                    "sort": { "field": "name", "order": "asc" },
                    "folder": { "libraryId": "golden", "path": "3D assets/Car Kit" },
                },
                "page": 0,
                "pageSize": 1000,
            }))
            .unwrap();
            let libs = &libs;
            async move {
                let result = search_libraries(&payload, &target_libraries(&payload.query, libs))
                    .await
                    .unwrap();
                let names: Vec<String> = result.assets.into_iter().map(|a| a.file_name).collect();
                (names, result.total.unwrap(), result.file_total.unwrap())
            }
        };
        let (names, items, files) = search("", false).await;
        println!("3D assets/Car Kit: {items} items ({files} files)");
        assert!(!names
            .iter()
            .any(|n| n == "License.txt" || n == "Preview.png"));
        let (shown, all, all_files) = search("", true).await;
        assert!(["License.txt", "Preview.png"]
            .iter()
            .all(|n| shown.iter().any(|s| s == n)));
        // The support files join the items, and group members stay hidden.
        let support: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM assets
             WHERE file_path LIKE '3D assets/Car Kit/%' AND support_role IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((all, all_files), (items + support, files));
        assert!(items + support < files);

        let payload: SearchPayload = serde_json::from_value(serde_json::json!({
            "query": { "text": "", "filter": {}, "sort": { "field": "name", "order": "asc" } },
            "page": 0,
            "pageSize": 1,
        }))
        .unwrap();
        let root = search_libraries(&payload, &target_libraries(&payload.query, &libs))
            .await
            .unwrap();
        println!(
            "library root: {} items ({} files)",
            root.total.unwrap(),
            root.file_total.unwrap()
        );
    }

    /// The library root's Packs view on a copy of the golden snapshot's library DB, named by
    /// `OPENDAM_GOLDEN_DB` as above, with the snapshot's license files read from `golden_root`:
    /// `cargo nextest run --run-ignored only golden_snapshot --no-capture`.
    #[tokio::test]
    #[ignore = "needs OPENDAM_GOLDEN_DB, a copy of the golden snapshot's library.sqlite"]
    async fn the_packs_view_of_the_golden_snapshot_shows_every_pack_with_a_cover() {
        use crate::commands::folder::packs_below;
        let file = std::env::var("OPENDAM_GOLDEN_DB").expect("OPENDAM_GOLDEN_DB is set");
        let pool = crate::db::connect_library_sqlite(Path::new(&file))
            .await
            .unwrap();
        let root = golden_root();
        crate::structure::rebuild(&pool, &root).await.unwrap();
        let library = LibraryHandle {
            id: "golden".to_string(),
            name: "golden".to_string(),
            root_path: root.clone(),
            pool: pool.clone(),
        };
        let started = std::time::Instant::now();
        let cards = packs_below(std::slice::from_ref(&library), "", false)
            .await
            .unwrap();
        println!("{} cards in {:?}", cards.len(), started.elapsed());
        let no_cover: Vec<&str> = cards
            .iter()
            .filter(|c| c.cover.is_none())
            .map(|c| c.path.as_str())
            .collect();
        let no_items: Vec<&str> = cards
            .iter()
            .filter(|c| c.item_count == 0)
            .map(|c| c.path.as_str())
            .collect();
        let named = cards
            .iter()
            .filter(|c| Some(c.name.as_str()) != c.path.rsplit('/').next())
            .count();
        println!("without a cover: {no_cover:?}");
        println!("{} with no visible items: {no_items:?}", no_items.len());
        println!("{named} named by their license header, unlike their folder");
        for card in cards.iter().take(5) {
            println!(
                "{} ({}): {:?}",
                card.name,
                card.parent_path,
                card.cover.as_ref().map(|c| &c.file_path)
            );
        }
        assert_eq!(cards.len(), 254);
        assert_eq!(no_cover, [] as [&str; 0]);
        assert_eq!(no_items.len(), 24);
        // The 11 zip-only folders in `Archive/` hold no indexed file, so they have no row.
        let archive: Vec<&str> = cards
            .iter()
            .map(|c| c.path.as_str())
            .filter(|p| p.starts_with("Archive/"))
            .collect();
        assert_eq!(
            archive,
            [
                "Archive/Mini Car Kit",
                "Archive/Onscreen Controls",
                "Archive/Pre-rendered Models",
                "Archive/Space Kit (Legacy)"
            ]
        );
        let names: Vec<String> = cards.iter().map(|c| c.name.to_ascii_lowercase()).collect();
        assert!(
            names.windows(2).all(|w| w[0] <= w[1]),
            "the cards are not in name order"
        );
        let car = cards
            .iter()
            .find(|c| c.path == "3D assets/Car Kit")
            .unwrap();
        assert_eq!(
            (car.name.as_str(), car.parent_path.as_str()),
            ("Car Kit", "3D assets")
        );
        // Every cover is an indexed image of its own pack.
        for card in &cards {
            let cover = card.cover.as_ref().unwrap();
            assert!(
                cover
                    .file_path
                    .starts_with(&format!("{}/{}/", root.display(), card.path)),
                "{} has cover {}",
                card.path,
                cover.file_path
            );
        }
        assert_eq!(
            crate::structure::rebuild(&pool, &root).await.unwrap(),
            0,
            "a rerun rewrote pack info"
        );
    }

    /// The rebuild each pack edit runs (A1: under 2 s) on a copy of the golden snapshot's
    /// library DB, named by `OPENDAM_GOLDEN_DB` as above, with the license files read from
    /// `golden_root`. It writes edits to the copy and clears them again. Time it as the app ships,
    /// optimized (a debug build takes about 1.3 s, and 2.1 s to restore a pack, whose license the
    /// pass reads again), keeping the debug-only test helpers:
    /// `CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true CARGO_PROFILE_RELEASE_LTO=false cargo nextest
    /// run --cargo-profile release --run-ignored only golden_snapshot_pack_edits --no-capture`.
    #[tokio::test]
    #[ignore = "needs OPENDAM_GOLDEN_DB, a copy of the golden snapshot's library.sqlite"]
    async fn golden_snapshot_pack_edits_each_rebuild_in_under_2_s() {
        use crate::commands::folder::{packs_below, set_pack_override_in, PackEdit};
        use crate::structure::PackMark;
        use std::time::{Duration, Instant};
        let file = std::env::var("OPENDAM_GOLDEN_DB").expect("OPENDAM_GOLDEN_DB is set");
        let pool = crate::db::connect_library_sqlite(Path::new(&file))
            .await
            .unwrap();
        let root = golden_root();
        let library = LibraryHandle {
            id: "golden".to_string(),
            name: "golden".to_string(),
            root_path: root.clone(),
            pool: pool.clone(),
        };
        let db = memory_database(vec![library.clone()]).await;
        crate::structure::rebuild(&pool, &root).await.unwrap();
        let before = crate::structure::tests::derived_rows(&pool).await;
        let cover: String = sqlx::query_scalar(
            "SELECT id FROM assets WHERE file_path = '3D assets/Car Kit/Previews/sedan-sports.png'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let mark = |mark| PackEdit::Mark { mark };
        let details = |name: Option<&str>| PackEdit::Details {
            name: name.map(str::to_string),
            license: None,
            source_url: None,
        };
        let edits = [
            (
                "Mark as pack",
                "3D assets/Platformer Kit/Models",
                mark(Some(PackMark::Pack)),
            ),
            (
                "Not a pack",
                "3D assets/Nature Kit",
                mark(Some(PackMark::NotPack)),
            ),
            ("Name", "3D assets/Car Kit", details(Some("My cars"))),
            (
                "Use as pack cover",
                "3D assets/Car Kit",
                PackEdit::Cover {
                    asset_id: Some(cover),
                },
            ),
            (
                "Clear the mark",
                "3D assets/Platformer Kit/Models",
                mark(None),
            ),
            ("Clear the mark", "3D assets/Nature Kit", mark(None)),
            ("Clear the name", "3D assets/Car Kit", details(None)),
            (
                "Clear the cover",
                "3D assets/Car Kit",
                PackEdit::Cover { asset_id: None },
            ),
        ];
        let mut slowest = Duration::ZERO;
        for (i, (what, path, edit)) in edits.into_iter().enumerate() {
            let started = Instant::now();
            set_pack_override_in(&db, "golden", path, edit)
                .await
                .unwrap();
            let took = started.elapsed();
            println!("{what} on {path}: {took:?} (target under 2s)");
            slowest = slowest.max(took);
            if i == 3 {
                let cards = packs_below(std::slice::from_ref(&library), "", false)
                    .await
                    .unwrap();
                let paths: Vec<&str> = cards.iter().map(|c| c.path.as_str()).collect();
                assert!(paths.contains(&"3D assets/Platformer Kit/Models"));
                assert!(!paths.contains(&"3D assets/Nature Kit"));
                let car = cards
                    .iter()
                    .find(|c| c.path == "3D assets/Car Kit")
                    .unwrap();
                assert_eq!(car.name, "My cars");
                assert!(car
                    .cover
                    .as_ref()
                    .unwrap()
                    .file_path
                    .ends_with("sedan-sports.png"));
                println!("{} packs with the edits", cards.len());
            }
        }
        assert!(
            slowest < Duration::from_secs(2),
            "slowest edit took {slowest:?}"
        );
        assert_eq!(crate::structure::tests::derived_rows(&pool).await, before);
    }

    /// Format groups on a copy of the golden snapshot's library DB, named by `OPENDAM_GOLDEN_DB`
    /// as above: every Kenney kit model shows once, as GLB, and its other formats still find
    /// it. It writes to the copy: it clears the groups for a first pass. Run it with
    /// `cargo nextest run --run-ignored only golden_snapshot_groups --no-capture`.
    #[tokio::test]
    #[ignore = "needs OPENDAM_GOLDEN_DB, a copy of the golden snapshot's library.sqlite"]
    async fn golden_snapshot_groups_show_each_kenney_model_once_as_glb() {
        use crate::commands::folder::child_folders;
        use std::collections::BTreeSet;
        use std::time::{Duration, Instant};
        let file = std::env::var("OPENDAM_GOLDEN_DB").expect("OPENDAM_GOLDEN_DB is set");
        let pool = crate::db::connect_library_sqlite(Path::new(&file))
            .await
            .unwrap();
        let count = |sql: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sql)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };

        // A first pass deriving every group, then a rerun writing nothing (A1).
        sqlx::query("UPDATE assets SET group_id = NULL; UPDATE folders SET split_groups = 0;")
            .execute(&pool)
            .await
            .unwrap();
        let started = Instant::now();
        let written = crate::structure::rebuild(&pool, &golden_root())
            .await
            .unwrap();
        let took = started.elapsed();
        println!("first group pass: {written} rows written in {took:?}");
        assert!(took < Duration::from_secs(10));
        let started = Instant::now();
        assert_eq!(
            crate::structure::rebuild(&pool, &golden_root())
                .await
                .unwrap(),
            0
        );
        let took = started.elapsed();
        println!("rerun: 0 rows written in {took:?}");
        assert!(took < Duration::from_secs(2));
        let mains = count("SELECT COUNT(*) FROM assets WHERE group_id = id").await;
        let grouped = count("SELECT COUNT(*) FROM assets WHERE group_id IS NOT NULL").await;
        println!("{mains} groups of {grouped} files");

        // In every kit with a `Models/GLB format/` folder, each model item is a GLB.
        let kits: Vec<(String, i64, i64)> = sqlx::query_as(
            "WITH kits AS (
                 SELECT DISTINCT substr(file_path, 1, instr(file_path, '/Models/GLB format/') - 1)
                     AS kit
                 FROM assets WHERE file_path LIKE '3D assets/%/Models/GLB format/%'
             )
             SELECT kit,
                 (SELECT COUNT(*) FROM assets a
                  WHERE a.file_path >= kit || '/Models/' AND a.file_path < kit || '/Models0'
                    AND a.kind = '3d_model' AND (a.group_id IS NULL OR a.group_id = a.id)),
                 (SELECT COUNT(*) FROM assets a
                  WHERE a.file_path >= kit || '/Models/' AND a.file_path < kit || '/Models0'
                    AND a.kind = '3d_model' AND (a.group_id IS NULL OR a.group_id = a.id)
                    AND a.extension <> 'glb')
             FROM kits ORDER BY kit",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let models: i64 = kits.iter().map(|k| k.1).sum();
        let not_glb: Vec<&(String, i64, i64)> = kits.iter().filter(|k| k.2 > 0).collect();
        println!(
            "{} kits with GLB format/: {models} model items, {} not GLB",
            kits.len(),
            not_glb.len()
        );
        assert!(kits.len() > 40);
        assert_eq!(not_glb, [] as [&(String, i64, i64); 0]);

        let libs = [LibraryHandle {
            id: "golden".to_string(),
            name: "golden".to_string(),
            root_path: PathBuf::from("/golden"),
            pool: pool.clone(),
        }];
        let search = |text: &str, folder: &str, filter: serde_json::Value| {
            let payload: SearchPayload = serde_json::from_value(serde_json::json!({
                "query": {
                    "text": text,
                    "filter": filter,
                    "sort": { "field": "name", "order": "asc" },
                    "folder": { "libraryId": "golden", "path": folder },
                },
                "page": 0,
                "pageSize": 10_000,
            }))
            .unwrap();
            let libs = &libs;
            async move {
                let result = search_libraries(&payload, &target_libraries(&payload.query, libs))
                    .await
                    .unwrap();
                let extensions: BTreeSet<String> =
                    result.assets.iter().map(|a| a.extension.clone()).collect();
                (result.total.unwrap(), extensions)
            }
        };
        let kit = "3D assets/Car Kit";
        let glbs = count(
            "SELECT COUNT(*) FROM assets
             WHERE file_path LIKE '3D assets/Car Kit/Models/GLB format/%' AND extension = 'glb'",
        )
        .await;
        let glb_set = BTreeSet::from(["glb".to_string()]);

        // Filtering by FBX still lists every model, each as its GLB.
        let fbx = search("", kit, serde_json::json!({ "extensions": ["fbx"] })).await;
        println!("{kit} filtered by FBX: {fbx:?}");
        assert_eq!(fbx, (glbs, glb_set.clone()));

        // `FBX format/` lists the models and their texture, and holds no GLB.
        let fbx_folder = format!("{kit}/Models/FBX format");
        let listed = search("", &fbx_folder, serde_json::json!({})).await;
        println!("{fbx_folder}: {listed:?}");
        assert_eq!(listed.0, glbs + 1);
        assert_eq!(
            search(
                "",
                &fbx_folder,
                serde_json::json!({ "extensions": ["glb"] })
            )
            .await
            .0,
            0
        );

        // "fbx" finds the kit's groups through their FBX copies' paths.
        let found = search("fbx", kit, serde_json::json!({})).await;
        println!("{kit}, \"fbx\": {found:?}");
        assert_eq!(found.0, glbs + 1);

        // Each format folder counts every model, `Models` counts each once, and each count is
        // the folder's grid total.
        let nodes = child_folders(&pool, &golden_root(), &format!("{kit}/Models"))
            .await
            .unwrap();
        let models_node = child_folders(&pool, &golden_root(), kit)
            .await
            .unwrap()
            .into_iter()
            .find(|f| f.name == "Models")
            .unwrap();
        println!(
            "{}: {}; {}",
            models_node.path,
            models_node.item_count,
            nodes
                .iter()
                .map(|f| format!("{} {}", f.name, f.item_count))
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert_eq!(models_node.item_count, glbs + 1);
        for node in nodes.iter().chain([&models_node]) {
            assert_eq!(node.item_count, glbs + 1, "{}", node.path);
            let total = search("", &node.path, serde_json::json!({})).await.0;
            assert_eq!(total, node.item_count, "{}", node.path);
        }
    }

    /// Kenney's renders on a copy of the golden snapshot's library DB, named by
    /// `OPENDAM_GOLDEN_DB` as above (A4): every render is a hidden bundled preview, each kit
    /// model's GLB records one, and every render is 64 px, too small to picture its model, so
    /// each such model has a thumbnail or waits for a preview job (small-bundled-previews A2,
    /// A3). It writes to the copy: it clears the renders and their sizes for a first pass. Run
    /// it with
    /// `cargo nextest run --run-ignored only golden_snapshot_renders --no-capture`.
    #[tokio::test]
    #[ignore = "needs OPENDAM_GOLDEN_DB, a copy of the golden snapshot's library.sqlite"]
    async fn golden_snapshot_renders_are_the_bundled_previews_of_their_models() {
        use std::time::{Duration, Instant};
        let file = std::env::var("OPENDAM_GOLDEN_DB").expect("OPENDAM_GOLDEN_DB is set");
        let pool = crate::db::connect_library_sqlite(Path::new(&file))
            .await
            .unwrap();
        let count = |sql: String| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(&sql)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };

        // A first pass deriving every render and reading its header, then a rerun reading none
        // and writing nothing (A1, small-bundled-previews A2).
        sqlx::query(
            "UPDATE assets SET bundled_preview = NULL, render_width = NULL, render_height = NULL,
                 render_read_bytes = NULL, render_read_mtime = NULL;
             UPDATE assets SET support_role = NULL WHERE support_role = 'model_preview';",
        )
        .execute(&pool)
        .await
        .unwrap();
        let started = Instant::now();
        let first = crate::structure::pass(&pool, &golden_root()).await.unwrap();
        let took = started.elapsed();
        println!(
            "first render pass: {} render headers read, {} rows written in {took:?}",
            first.render_reads, first.written
        );
        assert_eq!(first.render_reads, 3741);
        assert!(took < Duration::from_secs(10));
        let started = Instant::now();
        assert_eq!(
            crate::structure::pass(&pool, &golden_root()).await.unwrap(),
            crate::structure::PassRun {
                written: 0,
                render_reads: 0
            }
        );
        let took = started.elapsed();
        println!("rerun: no header read, 0 rows written in {took:?}");
        assert!(took < Duration::from_secs(2));

        // Every `Previews/` image is a hidden render that an item records, and only main items
        // and files on their own record one.
        let renders =
            count("SELECT COUNT(*) FROM assets WHERE file_path LIKE '%/Previews/%'".into()).await;
        let hidden = count(
            "SELECT COUNT(*) FROM assets
             WHERE file_path LIKE '%/Previews/%' AND support_role = 'model_preview'"
                .into(),
        )
        .await;
        let recorded = count(
            "SELECT COUNT(DISTINCT bundled_preview) FROM assets WHERE bundled_preview IS NOT NULL"
                .into(),
        )
        .await;
        let items =
            count("SELECT COUNT(*) FROM assets WHERE bundled_preview IS NOT NULL".into()).await;
        println!(
            "{renders} Previews/ images, {hidden} of them hidden renders, {recorded} recorded by {items} items"
        );
        assert_eq!((renders, hidden, recorded), (3741, 3741, 3741));
        assert_eq!(
            count(format!(
                "SELECT COUNT(*) FROM assets a WHERE a.bundled_preview IS NOT NULL
                 AND NOT {MAIN_OR_SINGLE}"
            ))
            .await,
            0
        );
        assert_eq!(
            count(format!(
                "SELECT COUNT(*) FROM assets a WHERE {NOT_SUPPORT} AND {MAIN_OR_SINGLE}
                 AND a.file_path LIKE '%/Previews/%'"
            ))
            .await,
            0
        );
        // An image in a render folder that renders no model stays an ordinary image.
        let unmatched: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT file_path, support_role FROM assets
             WHERE file_path LIKE '3D assets/Animated Characters Bundle/Preview/%'
             ORDER BY file_path",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        println!("unmatched: {unmatched:?}");
        assert_eq!(unmatched.len(), 4);
        assert!(unmatched.iter().all(|(_, role)| role.is_none()));

        // Every render is Kenney's 64 px one, so none pictures its model, and each model with one
        // has a thumbnail or waits for a preview job.
        let sizes: Vec<(i64, i64, i64)> = sqlx::query_as(
            "SELECT render_width, render_height, COUNT(*) FROM assets
             WHERE support_role = 'model_preview' GROUP BY 1, 2",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        println!("render sizes: {sizes:?}");
        assert_eq!(sizes, [(64, 64, 3741)]);
        assert_eq!(
            count(format!(
                "SELECT COUNT(*) FROM assets a WHERE {}",
                pictured("a")
            ))
            .await,
            0
        );
        let thumbnailed = count(
            "SELECT COUNT(*) FROM assets a WHERE a.bundled_preview IS NOT NULL
             AND a.thumbnail_path IS NOT NULL"
                .into(),
        )
        .await;
        let queued = count(format!(
            "SELECT COUNT(*) FROM assets a WHERE a.bundled_preview IS NOT NULL
             AND a.thumbnail_path IS NULL AND a.preview_status = 'pending'
             AND EXISTS (SELECT 1 FROM preview_jobs p
                  WHERE p.asset_id = a.id AND p.status IN {})",
            crate::preview::WAITING_JOBS
        ))
        .await;
        println!(
            "models with a render: {items}, {thumbnailed} with a thumbnail, {queued} waiting for one"
        );
        assert_eq!(thumbnailed + queued, items);

        // In every kit with a `Models/GLB format/` folder, each model item is a GLB, and each one
        // Kenney ships a render of shows it. Some kits also ship older models under other names
        // (`GLTF format/tower_edge.glb` beside `GLB format/tower-edge.glb`), with no render.
        let kits: Vec<(String, i64, i64, i64)> = sqlx::query_as(
            "WITH kits AS (
                 SELECT DISTINCT substr(file_path, 1, instr(file_path, '/Models/GLB format/') - 1)
                     AS kit
                 FROM assets WHERE file_path LIKE '3D assets/%/Models/GLB format/%'
             ),
             models AS (
                 SELECT kit, a.extension, a.bundled_preview,
                     EXISTS (SELECT 1 FROM assets r
                         WHERE r.file_path >= kit || '/Previews/' AND r.file_path < kit || '/Previews0'
                           AND lower(substr(r.file_name, 1, length(r.file_name) - length(r.extension) - 1))
                             = lower(substr(a.file_name, 1, length(a.file_name) - length(a.extension) - 1)))
                         AS rendered
                 FROM kits JOIN assets a
                     ON a.file_path >= kit || '/Models/' AND a.file_path < kit || '/Models0'
                 WHERE a.kind = '3d_model' AND (a.group_id IS NULL OR a.group_id = a.id)
             )
             SELECT kit, COUNT(*),
                 SUM(extension <> 'glb' OR (rendered AND bundled_preview IS NULL)),
                 SUM(NOT rendered)
             FROM models GROUP BY kit ORDER BY kit",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let models: i64 = kits.iter().map(|k| k.1).sum();
        let unrendered: i64 = kits.iter().map(|k| k.3).sum();
        let wrong: Vec<&(String, i64, i64, i64)> = kits.iter().filter(|k| k.2 > 0).collect();
        println!(
            "{} kits with GLB format/: {models} model items, {unrendered} with no render, kits with one not a GLB or missing its render: {wrong:?}",
            kits.len()
        );
        assert!(kits.len() > 40);
        assert_eq!(wrong, [] as [&(String, i64, i64, i64); 0]);

        let libs = [LibraryHandle {
            id: "golden".to_string(),
            name: "golden".to_string(),
            root_path: PathBuf::from("/golden"),
            pool: pool.clone(),
        }];
        let search = |folder: &str, show_hidden: bool| {
            let payload: SearchPayload = serde_json::from_value(serde_json::json!({
                "query": {
                    "text": "",
                    "filter": { "showHidden": show_hidden },
                    "sort": { "field": "name", "order": "asc" },
                    "folder": { "libraryId": "golden", "path": folder },
                },
                "page": 0,
                "pageSize": 10_000,
            }))
            .unwrap();
            let libs = &libs;
            async move {
                search_libraries(&payload, &target_libraries(&payload.query, libs))
                    .await
                    .unwrap()
            }
        };
        let kit = "3D assets/Car Kit";
        let found = search(kit, false).await;
        assert!(!found
            .assets
            .iter()
            .any(|a| a.file_path.contains("/Previews/")));
        let sedan = found
            .assets
            .iter()
            .find(|a| a.file_name == "sedan.glb")
            .expect("sedan.glb in the grid");
        let (render_thumb,): (Option<String>,) = sqlx::query_as(
            "SELECT thumbnail_path FROM assets WHERE file_path = '3D assets/Car Kit/Previews/sedan.png'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let picture = format!(
            "/golden/{}",
            render_thumb
                .as_deref()
                .unwrap_or("3D assets/Car Kit/Previews/sedan.png")
        );
        // Its 64 px render is too small to fit a card, so the card shows its own thumbnail and
        // keeps the render as a fallback (small-bundled-previews A1).
        println!(
            "sedan.glb shows {:?}, falls back to {:?}",
            sedan.thumbnail_path, sedan.fallback_render
        );
        assert_eq!(sedan.bundled_preview, None);
        assert_eq!(sedan.fallback_render.as_deref(), Some(picture.as_str()));
        assert!(sedan.thumbnail_path.is_some());
        let previews = format!("{kit}/Previews");
        let (hidden_total, shown_total) = (
            search(&previews, false).await.total,
            search(&previews, true).await.total,
        );
        println!("{previews}: {hidden_total:?} items, {shown_total:?} with hidden files");
        assert_eq!(hidden_total, Some(0));
        assert!(shown_total.unwrap() > 0);
    }

    /// The first page of the library root and of `3D assets`, as the grid asks for them (date
    /// order, 200 assets, with the total), timed on a copy of the golden snapshot's library DB
    /// named by `OPENDAM_GOLDEN_DB` (A5): `cargo nextest run --run-ignored only
    /// golden_snapshot_first_pages --no-capture`. Prints the median of 20 runs after 5 warm-ups.
    #[tokio::test]
    #[ignore = "needs OPENDAM_GOLDEN_DB, a copy of the golden snapshot's library.sqlite"]
    async fn golden_snapshot_first_pages_take_their_time() {
        use std::time::{Duration, Instant};
        let file = std::env::var("OPENDAM_GOLDEN_DB").expect("OPENDAM_GOLDEN_DB is set");
        let pool = crate::db::connect_library_sqlite(Path::new(&file))
            .await
            .unwrap();
        let libs = [LibraryHandle {
            id: "golden".to_string(),
            name: "golden".to_string(),
            root_path: PathBuf::from("/golden"),
            pool,
        }];
        for folder in [None, Some("3D assets")] {
            let mut query = serde_json::json!({
                "text": "",
                "filter": {},
                "sort": { "field": "date", "order": "desc" },
            });
            if let Some(path) = folder {
                query["folder"] = serde_json::json!({ "libraryId": "golden", "path": path });
            }
            let payload: SearchPayload = serde_json::from_value(serde_json::json!({
                "query": query,
                "page": 0,
                "pageSize": 200,
                "includeTotal": true,
            }))
            .unwrap();
            let targets = target_libraries(&payload.query, &libs);
            let mut times: Vec<Duration> = Vec::new();
            let mut total = None;
            for run in 0..25 {
                let started = Instant::now();
                let result = search_libraries(&payload, &targets).await.unwrap();
                if run >= 5 {
                    times.push(started.elapsed());
                }
                total = result.total;
            }
            times.sort();
            println!(
                "first page of {:?}: median {:?} (fastest {:?}, slowest {:?}), total {total:?}",
                folder.unwrap_or("the library root"),
                times[times.len() / 2],
                times[0],
                times[times.len() - 1]
            );
        }
    }

    /// Live counts (plan C23, A5) on a copy of the golden snapshot's library DB, named by
    /// `OPENDAM_GOLDEN_DB` as above. For a search for "tree", every count beside the tree, the
    /// Types filter and the format filter equals the grid's total, the Platformer Kit's format
    /// folders each count its models while `Models/` counts them once, and each node's folder
    /// counts take under 300 ms. Run it with
    /// `cargo nextest run --run-ignored only golden_snapshot_live_counts --no-capture`.
    #[tokio::test]
    #[ignore = "needs OPENDAM_GOLDEN_DB, a copy of the golden snapshot's library.sqlite"]
    async fn golden_snapshot_live_counts_equal_the_grid_totals_and_take_their_time() {
        use std::time::{Duration, Instant};
        let file = std::env::var("OPENDAM_GOLDEN_DB").expect("OPENDAM_GOLDEN_DB is set");
        let pool = crate::db::connect_library_sqlite(Path::new(&file))
            .await
            .unwrap();
        let libs = [LibraryHandle {
            id: "golden".to_string(),
            name: "golden".to_string(),
            root_path: PathBuf::from("/golden"),
            pool: pool.clone(),
        }];
        let search = || serde_json::json!({ "text": "tree" });
        let kit = "3D assets/Platformer Kit";
        let models = format!("{kit}/Models");
        let parents = ["", "3D assets", kit, models.as_str()];

        // Each node's counts as the tree asks for them when it expands: the median of 10 runs
        // after 2 warm-ups.
        let median = |mut times: Vec<Duration>| {
            times.sort();
            times[times.len() / 2]
        };
        for parent in parents {
            let mut times = Vec::new();
            for run in 0..12 {
                let started = Instant::now();
                live_folder_counts(&libs[0], search(), &[parent]).await;
                if run >= 2 {
                    times.push(started.elapsed());
                }
            }
            let took = median(times);
            println!("folder counts below {parent:?}: median {took:?} (target under 300 ms)");
            assert!(
                took < Duration::from_millis(300),
                "{parent:?} took {took:?}"
            );
        }
        let started = Instant::now();
        let counts = live_folder_counts(&libs[0], search(), &parents).await;
        println!(
            "folder counts below all four at once: {:?}",
            started.elapsed()
        );
        let started = Instant::now();
        let facets = live_facet_counts(&libs, search()).await;
        println!("kind and extension counts: {:?}", started.elapsed());

        let total = |patch: serde_json::Value| {
            let mut query = search();
            for (key, value) in patch.as_object().unwrap() {
                query[key] = value.clone();
            }
            let libs = &libs;
            async move { counted_search(libs, query).await.1.unwrap() }
        };
        let mut checked = 0;
        for parent in parents {
            let children: Vec<String> =
                sqlx::query_scalar("SELECT path FROM folders WHERE parent_path = ?")
                    .bind(parent)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert!(!children.is_empty(), "{parent:?} has no children");
            for child in children {
                let count = counts[parent].get(&child).copied().unwrap_or(0);
                let folder = serde_json::json!({ "libraryId": "golden", "path": child });
                let expected = total(serde_json::json!({ "folder": folder })).await;
                assert_eq!(count, expected, "folder {child:?}");
                checked += 1;
            }
        }
        for (kind, count) in &facets.kinds {
            let expected = total(serde_json::json!({ "filter": { "kinds": [kind] } })).await;
            assert_eq!(*count, expected, "kind {kind}");
        }
        for (ext, count) in &facets.extensions {
            let expected = total(serde_json::json!({ "filter": { "extensions": [ext] } })).await;
            assert_eq!(*count, expected, "extension {ext}");
        }
        // With FBX active, a format pill's count is the total once it joins FBX.
        let with_fbx =
            || serde_json::json!({ "text": "tree", "filter": { "extensions": ["fbx"] } });
        let union = live_facet_counts(&libs, with_fbx()).await;
        for ext in ["fbx", "glb", "gltf", "obj", "png", "svg", "jpg"] {
            let count = union.extension(ext);
            let mut query = with_fbx();
            query["filter"]["extensions"] = serde_json::json!(["fbx", ext]);
            let expected = counted_search(&libs, query).await.1.unwrap();
            assert_eq!(count, expected, "fbx plus {ext}");
        }
        println!(
            "with .fbx active: {:?}, {} for any other extension, each the total once it joins .fbx",
            union.extensions, union.extension_base
        );
        println!(
            "{checked} folder counts, {:?} by kind and {:?} by extension, each the grid's total",
            facets.kinds, facets.extensions
        );

        let in_models = counts[kit][&models];
        assert!(in_models > 0);
        for format in ["GLB format", "FBX format", "OBJ format"] {
            let path = format!("{models}/{format}");
            assert_eq!(counts[models.as_str()][&path], in_models, "{path}");
        }
        println!("{models} counts {in_models}, and so does each of its format folders");
    }

    #[tokio::test]
    async fn folder_items_are_the_grid_items_one_per_group_and_no_support_file() {
        use crate::structure::tests::{car_kit, SEDAN};
        const COLORMAP: &str = "3D assets/Car Kit/Models/GLB format/Textures/colormap.png";
        let pool = car_kit().await;
        let items = |folder: &'static str, untagged_only| {
            let pool = pool.clone();
            async move { folder_item_ids(&pool, folder, untagged_only).await.unwrap() }
        };

        // The root is the whole library: the sedan as its GLB, and the texture. The FBX and OBJ,
        // the render, the covers and the licenses never count.
        assert_eq!(items("", false).await, [COLORMAP, SEDAN[1]]);
        assert_eq!(
            items("3D assets/Car Kit", false).await,
            [COLORMAP, SEDAN[1]]
        );
        // The FBX's folder shows the sedan, as its main item that lies outside it.
        assert_eq!(
            items("3D assets/Car Kit/Models/FBX format/", false).await,
            [SEDAN[1]]
        );
        assert!(items("3D assets/Car Kit/Previews", false).await.is_empty());
        assert!(items("3D assets/Holiday Kit", false).await.is_empty());
        assert!(items("3D assets/Car", false).await.is_empty());
        // Every folder's badge, which the structure pass counts on its own, holds as many.
        let badges: Vec<(String, i64)> = sqlx::query_as("SELECT path, item_count FROM folders")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(badges.len() > 5);
        for (path, item_count) in badges {
            let n = folder_item_ids(&pool, &path, false).await.unwrap().len();
            assert_eq!(n as i64, item_count, "{path}");
        }

        insert_tag(&pool, "t-car", "Car").await;
        sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES (?, 't-car')")
            .bind(SEDAN[1])
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(items("3D assets/Car Kit", true).await, [COLORMAP]);
        assert_eq!(
            items("3D assets/Car Kit", false).await,
            [COLORMAP, SEDAN[1]]
        );
        assert!(items("3D assets/Car Kit/Models/FBX format", true)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn a_patch_on_a_group_reaches_its_three_formats_and_not_the_render() {
        use crate::structure::tests::{car_kit, each, rows, SEDAN, SEDAN_RENDER};
        let pool = car_kit().await;
        let db = memory_database(vec![memory_library("a", pool.clone())]).await;
        insert_tag(&db.master_pool, "car", "Car").await;
        let patch: AssetPatch = serde_json::from_value(serde_json::json!({
            "rating": 4, "colorLabel": "red", "isFavorite": true,
            "description": "four doors", "tags": ["car"],
            "previewStatus": "error",
        }))
        .unwrap();

        apply_asset_patch(&db, SEDAN[1], &patch).await.unwrap();

        let tags = "SELECT asset_id, tag_id FROM asset_tags";
        assert_eq!(rows(&pool, tags).await, each(&SEDAN, "car"));
        assert_eq!(
            rows(
                &pool,
                "SELECT id, rating || ' ' || color_label || ' ' || is_favorite || ' ' || description
                 FROM assets WHERE rating != 0 OR color_label != 'none' OR is_favorite
                     OR description != ''"
            )
            .await,
            each(&SEDAN, "4 red 1 four doors")
        );
        // The preview status is the item's own.
        assert_eq!(
            rows(
                &pool,
                "SELECT id, preview_status FROM assets WHERE preview_status = 'error'"
            )
            .await,
            each(&SEDAN[1..2], "error")
        );
        let render: Vec<String> = sqlx::query_scalar(
            "SELECT rating || ' ' || color_label || ' ' || is_favorite FROM assets WHERE id = ?",
        )
        .bind(SEDAN_RENDER)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(render, ["0 none 0"]);

        // Clearing the tags from a member's id clears them from the whole group.
        let clear: AssetPatch = serde_json::from_value(serde_json::json!({ "tags": [] })).unwrap();
        apply_asset_patch(&db, SEDAN[0], &clear).await.unwrap();
        assert!(rows(&pool, tags).await.is_empty());
    }
}
