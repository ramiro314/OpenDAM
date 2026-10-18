use serde_json;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tauri::{Emitter, State};

use crate::db::{make_relative_path, resolve_path, upsert_tag, Database, LibraryHandle};
use crate::models::{Asset, AssetPatch, FilterSpec, SearchPayload, SearchQuery, SearchResult};
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

    let resolved_file_path = match root {
        Some(r) => resolve_path(r, &row.file_path),
        None => row.file_path,
    };
    let resolved_thumb_path = match (root, row.thumbnail_path) {
        (Some(r), Some(t)) => Some(resolve_path(r, &t)),
        (_, t) => t,
    };

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
    }
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
    let filter = search_filter(&payload.query);
    let sort_field = payload.query.sort.field.as_str();
    let is_asc = payload.query.sort.order == "asc";
    let order_by = order_by(sort_field, is_asc);

    if target_libs.len() == 1 {
        let (pool, root_opt) = &target_libs[0];
        let total = if payload.include_total {
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
            total,
            page: payload.page,
            page_size: payload.page_size,
        });
    }

    // Multiple libraries: each returns only the rowid and sort key of its first `offset + limit`
    // matches, and only the merged page's rows are fetched in full.
    let mut total: i64 = 0;
    if payload.include_total {
        for (pool, _) in target_libs {
            if let Ok(c) = count_assets(pool, &filter).await {
                total += c;
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
        total: payload.include_total.then_some(total),
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
        for b in &filter.binds {
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
#[derive(Default)]
struct SqlFilter {
    conditions: Vec<String>,
    binds: Vec<String>,
}

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

    fn where_clause(&self) -> String {
        if self.conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", self.conditions.join(" AND "))
        }
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

fn search_filter(query: &SearchQuery) -> SqlFilter {
    let f = &query.filter;
    let mut filter = SqlFilter::default();

    if let Some(fts) = fts_match_expression(&query.text) {
        filter.push(
            "a.id IN (SELECT id FROM assets_fts WHERE assets_fts MATCH ?)",
            [fts],
        );
    }
    filter.push_in("a.kind", &f.kinds);
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
    filter.push_in("a.extension", &f.extensions);
    if let Some(folder) = &query.folder {
        push_folder_range(&mut filter, &folder.path);
    }
    push_tag_filter(&mut filter, f);
    push_collection_filter(&mut filter, query);

    // Smart filter conditions
    fn trimmed(s: &Option<String>) -> Option<&str> {
        s.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    if let Some(dir) = &f.directory {
        push_folder_range(&mut filter, dir);
    }
    if let Some(prefix) = trimmed(&f.name_prefix) {
        filter.push("a.file_name LIKE ?", [format!("{prefix}%")]);
    }
    if let Some(suffix) = trimmed(&f.name_suffix) {
        filter.push("a.file_name LIKE ?", [format!("%{suffix}")]);
    }
    filter
}

/// Requires files below `folder`, a path relative to the library root, recursively. The range
/// `p/ <= file_path < p0` (`0` follows `/`) holds exactly the paths starting with `p/`, uses the
/// unique `file_path` index and needs no `LIKE` escaping. The root (`""`, or only slashes) adds
/// no condition.
fn push_folder_range(filter: &mut SqlFilter, folder: &str) {
    let path = folder.trim().trim_matches('/');
    if !path.is_empty() {
        filter.push(
            "a.file_path >= ? AND a.file_path < ?",
            [format!("{path}/"), format!("{path}0")],
        );
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

async fn count_assets(pool: &sqlx::Pool<sqlx::Sqlite>, filter: &SqlFilter) -> sqlx::Result<i64> {
    #[cfg(test)]
    COUNT_QUERIES.with(|c| c.set(c.get() + 1));
    let sql = format!("SELECT COUNT(*) FROM assets a {}", filter.where_clause());
    let mut query = sqlx::query_scalar::<_, i64>(&sql);
    for b in &filter.binds {
        query = query.bind(b.clone());
    }
    query.fetch_one(pool).await
}

/// The columns of a `RawAssetRow` selected from `assets a`.
const ASSET_COLUMNS: &str = r#"
    a.id, a.file_path, a.file_name, a.extension, a.kind,
    a.size_bytes, a.modified_at, a.indexed_at, a.rating,
    a.color_label, a.is_favorite, a.description, a.thumbnail_path,
    a.preview_status, a.metadata,
    (SELECT json_group_array(tag_id) FROM asset_tags WHERE asset_id = a.id) AS tags,
    (SELECT json_group_array(collection_id) FROM collection_assets WHERE asset_id = a.id) AS collections"#;

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
        "SELECT a.rowid AS rowid, {ASSET_COLUMNS} FROM assets a WHERE a.rowid IN ({})",
        placeholders(rowids.len())
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
        r#"SELECT {ASSET_COLUMNS}
           FROM assets a
           {}
           ORDER BY {}
           LIMIT ?{}"#,
        filter.where_clause(),
        order_by,
        if offset.is_some() { " OFFSET ?" } else { "" }
    );
    let mut query = sqlx::query_as::<_, RawAssetRow>(&sql);
    for b in &filter.binds {
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
        let row = sqlx::query_as::<_, RawAssetRow>(
            r#"SELECT a.id, a.file_path, a.file_name, a.extension, a.kind,
               a.size_bytes, a.modified_at, a.indexed_at, a.rating,
               a.color_label, a.is_favorite, a.description, a.thumbnail_path,
               a.preview_status, a.metadata,
               (SELECT json_group_array(tag_id) FROM asset_tags WHERE asset_id = a.id) AS tags,
               (SELECT json_group_array(collection_id) FROM collection_assets WHERE asset_id = a.id) AS collections
               FROM assets a WHERE a.id = ?"#,
        )
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

#[tauri::command]
pub async fn update_asset(
    id: String,
    patch: AssetPatch,
    state: State<'_, Arc<AppState>>,
) -> Result<Asset> {
    apply_asset_patch(&state.db, &id, &patch).await?;
    get_asset(id, state).await
}

/// Writes the patch to the asset's own library only, memberships included.
async fn apply_asset_patch(db: &Database, id: &str, patch: &AssetPatch) -> Result<()> {
    let lib = db
        .library_of_asset(id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Asset not found: {}", id))?;
    // One transaction, so a patch whose tags fail saves none of its other fields either.
    let mut tx = lib.pool.begin().await.map_err(|e| e.to_string())?;

    if let Some(rating) = patch.rating {
        sqlx::query("UPDATE assets SET rating = ? WHERE id = ?")
            .bind(rating)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }
    if let Some(ref color_label) = patch.color_label {
        sqlx::query("UPDATE assets SET color_label = ? WHERE id = ?")
            .bind(color_label)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }
    if let Some(is_fav) = patch.is_favorite {
        let fav_int: i64 = if is_fav { 1 } else { 0 };
        sqlx::query("UPDATE assets SET is_favorite = ? WHERE id = ?")
            .bind(fav_int)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    }
    if let Some(ref description) = patch.description {
        sqlx::query("UPDATE assets SET description = ? WHERE id = ?")
            .bind(description)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
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
        sqlx::query("DELETE FROM asset_tags WHERE asset_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
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
            sqlx::query("INSERT OR IGNORE INTO asset_tags (asset_id, tag_id) VALUES (?, ?)")
                .bind(id)
                .bind(tag_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
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
    delete_assets_with(
        &state.db,
        std::slice::from_ref(&id),
        delete_file.unwrap_or(true),
        |path| trash::delete(path),
    )
    .await
    .map_err(|e| e.message)?;
    let _ = state.app_handle.emit("library:updated", ());
    Ok(())
}

/// Deletes an asset's rows and, when `delete_file` is set, moves its file to the trash with
/// `trash`. If the trash fails, it returns the error and leaves the file and the rows alone.
/// Returns the library that held the asset, whose structure pass the caller reruns.
async fn delete_asset_with<E: std::fmt::Display>(
    db: &Database,
    id: &str,
    delete_file: bool,
    trash: impl Fn(&Path) -> std::result::Result<(), E>,
) -> Result<Option<LibraryHandle>> {
    let libs = db.get_all_libraries().await;
    let mut target_pools = Vec::new();
    let mut file_to_delete: Option<std::path::PathBuf> = None;
    let mut holder = None;

    for lib in &libs {
        let row: Option<(String,)> = sqlx::query_as("SELECT file_path FROM assets WHERE id = ?")
            .bind(id)
            .fetch_optional(&lib.pool)
            .await
            .unwrap_or(None);

        if let Some((fp,)) = row {
            let abs_path = if Path::new(&fp).is_absolute() {
                std::path::PathBuf::from(fp)
            } else {
                lib.root_path.join(fp)
            };
            file_to_delete = Some(abs_path);
            target_pools.push(lib.pool.clone());
            holder = Some(lib.clone());
        }
    }

    target_pools.push(db.master_pool.clone());

    // Move the file to the trash. Never fall back to a permanent delete.
    if delete_file {
        if let Some(ref path) = file_to_delete {
            if path.exists() {
                trash(path).map_err(|e| {
                    format!(
                        "Could not move {} to the trash, so it was not deleted: {}",
                        path.display(),
                        e
                    )
                })?;
            }
        }
    }

    // Delete records from databases
    for pool in target_pools {
        let _ = sqlx::query("DELETE FROM asset_tags WHERE asset_id = ?")
            .bind(id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM collection_assets WHERE asset_id = ?")
            .bind(id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM preview_jobs WHERE asset_id = ?")
            .bind(id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM assets WHERE id = ?")
            .bind(id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("UPDATE libraries SET asset_count = (SELECT COUNT(*) FROM assets)")
            .execute(&pool)
            .await;
    }

    Ok(holder)
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

/// Deletes each asset with `delete_asset_with`, carrying on past failures, and reports them.
/// Before it returns, it reruns the structure pass once for each library it deleted from.
async fn delete_assets_with<E: std::fmt::Display>(
    db: &Database,
    ids: &[String],
    delete_file: bool,
    trash: impl Fn(&Path) -> std::result::Result<(), E>,
) -> std::result::Result<(), DeleteAssetsError> {
    let mut failures = Vec::new();
    let mut changed: Vec<LibraryHandle> = Vec::new();
    for id in ids {
        match delete_asset_with(db, id, delete_file, &trash).await {
            Ok(Some(lib)) if !changed.iter().any(|l| l.id == lib.id) => changed.push(lib),
            Ok(_) => {}
            Err(e) => failures.push((id.clone(), e)),
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
    rename_asset_with(&state.db, &id, &new_name).await?;
    let _ = state.app_handle.emit("library:updated", ());
    get_asset(id, state).await
}

/// Renames an asset's file in its folder and its rows, then reruns its library's structure pass.
async fn rename_asset_with(db: &Database, id: &str, new_name: &str) -> Result<()> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return Err("Asset file name cannot be empty, '.' or '..'".into());
    }

    if trimmed.contains(['/', '\0']) {
        return Err("File name cannot contain '/' or NUL".into());
    }

    let libs = db.get_all_libraries().await;
    let mut found_lib = None;
    let mut current_fp = None;
    let mut current_ext = None;

    for lib in &libs {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT file_path, extension FROM assets WHERE id = ?")
                .bind(id)
                .fetch_optional(&lib.pool)
                .await
                .unwrap_or(None);

        if let Some((fp, ext)) = row {
            found_lib = Some(lib.clone());
            current_fp = Some(fp);
            current_ext = Some(ext);
            break;
        }
    }

    let (lib, old_rel_fp, old_ext) = match (found_lib, current_fp, current_ext) {
        (Some(l), Some(fp), Some(ext)) => (l, fp, ext),
        _ => return Err(format!("Asset not found: {}", id)),
    };

    let old_abs_path = if Path::new(&old_rel_fp).is_absolute() {
        std::path::PathBuf::from(&old_rel_fp)
    } else {
        lib.root_path.join(&old_rel_fp)
    };

    let parent_dir = old_abs_path
        .parent()
        .ok_or_else(|| "Could not determine parent directory".to_string())?;

    // Determine new filename and extension
    let (final_name, new_ext) = if let Some((stem, ext)) = trimmed.rsplit_once('.') {
        if !stem.is_empty() && !ext.is_empty() {
            (trimmed.to_string(), ext.to_lowercase())
        } else {
            (format!("{}.{}", trimmed, old_ext), old_ext.clone())
        }
    } else {
        (format!("{}.{}", trimmed, old_ext), old_ext.clone())
    };

    let new_abs_path = parent_dir.join(&final_name);

    if new_abs_path.exists() && new_abs_path != old_abs_path {
        return Err(format!(
            "A file named '{}' already exists in this folder",
            final_name
        ));
    }

    // Rename file on disk
    let renamed_on_disk = old_abs_path.exists();
    if renamed_on_disk {
        std::fs::rename(&old_abs_path, &new_abs_path)
            .map_err(|e| format!("Failed to rename file on disk: {}", e))?;
    }

    let new_rel_fp = make_relative_path(&lib.root_path, &new_abs_path);
    let now = chrono::Utc::now().timestamp_millis();

    // Update in all relevant databases. The library's row must follow the file: if it can't
    // (a busy NAS), the file takes its old name back, so the index never sees a new file.
    let update = |pool: sqlx::Pool<sqlx::Sqlite>| {
        let (name, path, ext) = (final_name.clone(), new_rel_fp.clone(), new_ext.clone());
        async move {
            sqlx::query(
                "UPDATE assets SET file_name = ?, file_path = ?, extension = ?, modified_at = ? WHERE id = ?",
            )
            .bind(name)
            .bind(path)
            .bind(ext)
            .bind(now)
            .bind(id)
            .execute(&pool)
            .await
        }
    };
    if let Err(e) = update(lib.pool.clone()).await {
        if renamed_on_disk {
            if let Err(undo) = std::fs::rename(&new_abs_path, &old_abs_path) {
                return Err(format!(
                    "Could not record the rename ({e}), and could not undo it on disk: {undo}"
                ));
            }
        }
        return Err(format!(
            "Could not record the rename, so it was undone: {e}"
        ));
    }
    let _ = update(db.master_pool.clone()).await;

    structure::rebuild_library(db, &lib).await;
    Ok(())
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

        let result = delete_asset_with(&db, "tree", true, |_| Err("trash unavailable")).await;

        let file_exists = root.join("tree.glb").exists();
        let row_kept = has_row(&pool, "tree").await;
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(result.map(|_| ()), Err(trash_error(&root, "tree")));
        assert!(file_exists);
        assert!(row_kept);
    }

    #[tokio::test]
    async fn a_successful_trash_trashes_the_file_and_deletes_the_asset_row() {
        let (root, db, pool) = library_with(&["tree"]).await;
        let trashed = RefCell::new(Vec::new());

        let result = delete_asset_with(&db, "tree", true, |path| {
            trashed.borrow_mut().push(path.to_path_buf());
            Ok::<(), &str>(())
        })
        .await;

        let row_kept = has_row(&pool, "tree").await;
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(result.map(|lib| lib.map(|l| l.id)), Ok(Some("lib".into())));
        assert_eq!(trashed.into_inner(), vec![root.join("tree.glb")]);
        assert!(!row_kept);
    }

    /// A library in a temporary root holding a file at each of `paths`, indexed as an asset whose
    /// id is its path, with its first structure pass run.
    async fn library_in_folders(paths: &[&str]) -> (PathBuf, Database, Pool<Sqlite>) {
        let (root, db, pool) = library_with(&[]).await;
        for path in paths {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "glTF").unwrap();
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
                 VALUES (?, ?, ?, 'glb', 'model', 4, 0, 0)",
            )
            .bind(path)
            .bind(path)
            .bind(path.rsplit('/').next().unwrap())
            .execute(&pool)
            .await
            .unwrap();
        }
        crate::structure::rebuild(&pool).await.unwrap();
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

        let folders = crate::structure::tests::folder_rows(&pool).await;
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(result, Ok(()));
        let rest = [FOLDER_FILES[0], FOLDER_FILES[3]];
        assert_eq!(folders, crate::structure::tests::fresh_pass(&rest).await);
        assert!(!folders
            .iter()
            .any(|f| f.0.starts_with("3D assets/Nature Kit")));
    }

    #[tokio::test]
    async fn renaming_an_asset_leaves_the_folders_a_fresh_pass_would() {
        let (root, db, pool) = library_in_folders(&FOLDER_FILES).await;

        let result = rename_asset_with(&db, FOLDER_FILES[2], "boulder").await;

        let folders = crate::structure::tests::folder_rows(&pool).await;
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
    async fn a_rename_the_library_db_refuses_is_undone_on_disk() {
        let (root, db, pool) = library_in_folders(&FOLDER_FILES).await;
        sqlx::query(
            "CREATE TRIGGER refuse BEFORE UPDATE ON assets BEGIN SELECT RAISE(ABORT, 'busy'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let result = rename_asset_with(&db, FOLDER_FILES[3], "oak").await;

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
            " /3D assets/Platformer Kit/ ",
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
        for root in ["", "/", " "] {
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
                    .total
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
                "{folder}: search total {found}, SQL count {expected}, files in prefix-sharing siblings {siblings}"
            );
            assert!(expected > 0);
            assert_eq!(found, expected);
        }
        let root = total("").await;
        let all = count(&pool, "SELECT COUNT(*) FROM assets WHERE ?1 = ''", "").await;
        println!("library root: search total {root}, SQL count {all}");
        assert_eq!(root, all);
    }

    /// The structure pass's targets (A1) and the tree's node load time (A5) on a copy of the
    /// golden snapshot's library DB, named by `OPENDAM_GOLDEN_DB` as above. It writes to the copy:
    /// it clears `folders` for a first pass, then deletes and renames one asset. Run it with
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

        sqlx::query("DELETE FROM folders")
            .execute(&pool)
            .await
            .unwrap();
        let started = Instant::now();
        let written = crate::structure::rebuild(&pool).await.unwrap();
        timed(
            &format!("first pass, {written} rows written"),
            started.elapsed(),
            Duration::from_secs(10),
        );
        let started = Instant::now();
        let written = crate::structure::rebuild(&pool).await.unwrap();
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
            let children = child_folders(&pool, &parent).await.unwrap();
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
        let top = child_folders(&pool, "").await.unwrap();
        println!(
            "top-level folders: {}",
            top.iter()
                .map(|f| format!("{} ({})", f.name, f.item_count))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let kit = child_folders(&pool, "3D assets")
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

        // A rename and a delete each rerun the pass before they return.
        let (id, path): (String, String) = sqlx::query_as(
            "SELECT id, file_path FROM assets WHERE file_path LIKE '3D assets/Platformer Kit/%'
             ORDER BY file_path LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let started = Instant::now();
        rename_asset_with(&db, &id, "renamed-by-golden-test")
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
        let after = child_folders(&pool, "3D assets")
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
        assert_eq!(crate::structure::rebuild(&pool).await.unwrap(), 0);
    }
}
