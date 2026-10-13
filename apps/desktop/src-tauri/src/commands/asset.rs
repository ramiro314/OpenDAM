use serde_json;
use std::path::Path;
use std::sync::Arc;
use tauri::{Emitter, State};

use crate::db::{make_relative_path, resolve_path, Database};
use crate::models::{Asset, AssetPatch, SearchPayload, SearchResult};
use crate::AppState;

type Result<T> = std::result::Result<T, String>;

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
    let target_libs: Vec<(sqlx::Pool<sqlx::Sqlite>, Option<std::path::PathBuf>)> = {
        let mut list = Vec::new();
        if let Some(ref lids) = payload.query.library_ids {
            if !lids.is_empty() {
                for lib in &all_libs {
                    if lids.contains(&lib.id) {
                        list.push((lib.pool.clone(), Some(lib.root_path.clone())));
                    }
                }
            }
        } else if let Some(ref lid) = payload.query.library_id {
            if !lid.is_empty() {
                for lib in &all_libs {
                    if &lib.id == lid {
                        list.push((lib.pool.clone(), Some(lib.root_path.clone())));
                    }
                }
            }
        }

        if list.is_empty() {
            for lib in &all_libs {
                list.push((lib.pool.clone(), Some(lib.root_path.clone())));
            }
        }
        list
    };

    search_libraries(&payload, &target_libs).await
}

async fn search_libraries(
    payload: &SearchPayload,
    target_libs: &[(sqlx::Pool<sqlx::Sqlite>, Option<std::path::PathBuf>)],
) -> Result<SearchResult> {
    let offset = payload.page * payload.page_size;
    let limit = payload.page_size;

    let f = &payload.query.filter;

    let mut conditions: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    if let Some(fts) = fts_match_expression(&payload.query.text) {
        conditions.push("a.id IN (SELECT id FROM assets_fts WHERE assets_fts MATCH ?)".into());
        binds.push(fts);
    }

    if let Some(kinds) = &f.kinds {
        if !kinds.is_empty() {
            let placeholders = kinds.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            conditions.push(format!("a.kind IN ({})", placeholders));
            binds.extend(kinds.clone());
        }
    }

    if let Some(color_labels) = &f.color_labels {
        if !color_labels.is_empty() {
            let placeholders = color_labels
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            conditions.push(format!("a.color_label IN ({})", placeholders));
            binds.extend(color_labels.clone());
        }
    }

    if let Some(rating) = &f.rating {
        conditions.push("a.rating >= ? AND a.rating <= ?".into());
        binds.push(rating.min.to_string());
        binds.push(rating.max.to_string());
    }

    if let Some(is_fav) = f.is_favorite {
        if is_fav {
            conditions.push("a.is_favorite = 1".into());
        } else {
            conditions.push("a.is_favorite = 0".into());
        }
    }

    if let Some(dr) = &f.date_range {
        if let Some(from) = dr.from {
            conditions.push("a.modified_at >= ?".into());
            binds.push(from.to_string());
        }
        if let Some(to) = dr.to {
            conditions.push("a.modified_at <= ?".into());
            binds.push(to.to_string());
        }
    }

    if let Some(sr) = &f.size_range {
        if let Some(min) = sr.min {
            conditions.push("a.size_bytes >= ?".into());
            binds.push(min.to_string());
        }
        if let Some(max) = sr.max {
            conditions.push("a.size_bytes <= ?".into());
            binds.push(max.to_string());
        }
    }

    if let Some(ps) = &f.preview_status {
        if !ps.is_empty() {
            if ps == "missing" || ps == "needs_preview" {
                conditions.push("a.preview_status IN ('none', 'error')".into());
            } else if ps == "has_preview" {
                conditions.push("a.preview_status = 'done'".into());
            } else {
                conditions.push("a.preview_status = ?".into());
                binds.push(ps.clone());
            }
        }
    }

    if let Some(exts) = &f.extensions {
        if !exts.is_empty() {
            let placeholders = exts.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            conditions.push(format!("a.extension IN ({})", placeholders));
            binds.extend(exts.clone());
        }
    }

    // Tag filter: supports logic "and" (has all tags), "or" (has any tag), "and_or" (has primary tag AND any of remaining), "none" (untagged)
    let tag_logic = f.tag_logic.as_deref().unwrap_or("or");
    if tag_logic == "none" {
        conditions.push("a.id NOT IN (SELECT asset_id FROM asset_tags)".into());
    } else if let Some(tags) = &f.tags {
        if !tags.is_empty() {
            if tag_logic == "and" {
                let placeholders = tags.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                conditions.push(format!(
                    "a.id IN (SELECT asset_id FROM asset_tags WHERE tag_id IN ({}) GROUP BY asset_id HAVING COUNT(DISTINCT tag_id) = ?)",
                    placeholders
                ));
                binds.extend(tags.clone());
                binds.push(tags.len().to_string());
            } else if tag_logic == "and_or" {
                if tags.len() <= 1 {
                    conditions
                        .push("a.id IN (SELECT asset_id FROM asset_tags WHERE tag_id = ?)".into());
                    binds.push(tags[0].clone());
                } else {
                    let primary_tag = &tags[0];
                    let remaining_tags = &tags[1..];
                    let placeholders = remaining_tags
                        .iter()
                        .map(|_| "?")
                        .collect::<Vec<_>>()
                        .join(",");
                    conditions.push(format!(
                        "a.id IN (SELECT asset_id FROM asset_tags WHERE tag_id = ?) AND a.id IN (SELECT asset_id FROM asset_tags WHERE tag_id IN ({}))",
                        placeholders
                    ));
                    binds.push(primary_tag.clone());
                    binds.extend(remaining_tags.iter().cloned());
                }
            } else {
                let placeholders = tags.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                conditions.push(format!(
                    "a.id IN (SELECT asset_id FROM asset_tags WHERE tag_id IN ({}))",
                    placeholders
                ));
                binds.extend(tags.clone());
            }
        }
    }

    // Collection filter: supports multi-collection, subcollections, logic "and" (in all), "or" (in any), "and_or" (primary AND any remaining), "none" (unorganized)
    let mut target_collections: Vec<String> = Vec::new();
    if let Some(col_id) = payload
        .query
        .collection_id
        .as_ref()
        .or(f.collection_id.as_ref())
    {
        if !col_id.is_empty() && !target_collections.contains(col_id) {
            target_collections.push(col_id.clone());
        }
    }
    if let Some(cols) = &f.collection_ids {
        for cid in cols {
            if !cid.is_empty() && !target_collections.contains(cid) {
                target_collections.push(cid.clone());
            }
        }
    }

    let col_logic = f.collection_logic.as_deref().unwrap_or("or");
    let include_sub = f
        .include_subcollections
        .unwrap_or_else(|| payload.query.include_subcollections.unwrap_or(true));

    if col_logic == "none" || f.unorganized == Some(true) {
        conditions.push("a.id NOT IN (SELECT asset_id FROM collection_assets)".into());
    } else if !target_collections.is_empty() {
        if col_logic == "and" {
            for cid in &target_collections {
                if include_sub {
                    conditions.push(
                        "a.id IN (
                            WITH RECURSIVE col_tree AS (
                                SELECT id FROM collections WHERE id = ?
                                UNION ALL
                                SELECT c.id FROM collections c JOIN col_tree ct ON c.parent_id = ct.id
                            )
                            SELECT asset_id FROM collection_assets WHERE collection_id IN (SELECT id FROM col_tree)
                        )".into()
                    );
                } else {
                    conditions.push(
                        "a.id IN (SELECT asset_id FROM collection_assets WHERE collection_id = ?)"
                            .into(),
                    );
                }
                binds.push(cid.clone());
            }
        } else if col_logic == "and_or" {
            if target_collections.len() <= 1 {
                let cid = &target_collections[0];
                if include_sub {
                    conditions.push(
                        "a.id IN (
                            WITH RECURSIVE col_tree AS (
                                SELECT id FROM collections WHERE id = ?
                                UNION ALL
                                SELECT c.id FROM collections c JOIN col_tree ct ON c.parent_id = ct.id
                            )
                            SELECT asset_id FROM collection_assets WHERE collection_id IN (SELECT id FROM col_tree)
                        )".into()
                    );
                } else {
                    conditions.push(
                        "a.id IN (SELECT asset_id FROM collection_assets WHERE collection_id = ?)"
                            .into(),
                    );
                }
                binds.push(cid.clone());
            } else {
                let primary_col = &target_collections[0];
                let remaining_cols = &target_collections[1..];
                if include_sub {
                    conditions.push(
                        "a.id IN (
                            WITH RECURSIVE col_tree AS (
                                SELECT id FROM collections WHERE id = ?
                                UNION ALL
                                SELECT c.id FROM collections c JOIN col_tree ct ON c.parent_id = ct.id
                            )
                            SELECT asset_id FROM collection_assets WHERE collection_id IN (SELECT id FROM col_tree)
                        )".into()
                    );
                    binds.push(primary_col.clone());

                    let rem_placeholders = remaining_cols
                        .iter()
                        .map(|_| "?")
                        .collect::<Vec<_>>()
                        .join(",");
                    conditions.push(format!(
                        "a.id IN (
                            WITH RECURSIVE col_tree AS (
                                SELECT id FROM collections WHERE id IN ({})
                                UNION ALL
                                SELECT c.id FROM collections c JOIN col_tree ct ON c.parent_id = ct.id
                            )
                            SELECT asset_id FROM collection_assets WHERE collection_id IN (SELECT id FROM col_tree)
                        )",
                        rem_placeholders
                    ));
                    binds.extend(remaining_cols.iter().cloned());
                } else {
                    conditions.push(
                        "a.id IN (SELECT asset_id FROM collection_assets WHERE collection_id = ?)"
                            .into(),
                    );
                    binds.push(primary_col.clone());

                    let rem_placeholders = remaining_cols
                        .iter()
                        .map(|_| "?")
                        .collect::<Vec<_>>()
                        .join(",");
                    conditions.push(format!(
                        "a.id IN (SELECT asset_id FROM collection_assets WHERE collection_id IN ({}))",
                        rem_placeholders
                    ));
                    binds.extend(remaining_cols.iter().cloned());
                }
            }
        } else {
            if include_sub {
                let placeholders = target_collections
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(",");
                conditions.push(format!(
                    "a.id IN (
                        WITH RECURSIVE col_tree AS (
                            SELECT id FROM collections WHERE id IN ({})
                            UNION ALL
                            SELECT c.id FROM collections c JOIN col_tree ct ON c.parent_id = ct.id
                        )
                        SELECT asset_id FROM collection_assets WHERE collection_id IN (SELECT id FROM col_tree)
                    )",
                    placeholders
                ));
                binds.extend(target_collections);
            } else {
                let placeholders = target_collections
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(",");
                conditions.push(format!(
                    "a.id IN (SELECT asset_id FROM collection_assets WHERE collection_id IN ({}))",
                    placeholders
                ));
                binds.extend(target_collections);
            }
        }
    }

    // Smart filter conditions
    if let Some(dir) = &f.directory {
        if !dir.trim().is_empty() {
            conditions.push("a.file_path LIKE ?".into());
            binds.push(format!("%{}%", dir.trim()));
        }
    }

    if let Some(prefix) = &f.name_prefix {
        if !prefix.trim().is_empty() {
            conditions.push("a.file_name LIKE ?".into());
            binds.push(format!("{}%", prefix.trim()));
        }
    }

    if let Some(suffix) = &f.name_suffix {
        if !suffix.trim().is_empty() {
            conditions.push("a.file_name LIKE ?".into());
            binds.push(format!("%{}", suffix.trim()));
        }
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sort_col = match payload.query.sort.field.as_str() {
        "name" => "a.file_name",
        "size" => "a.size_bytes",
        "rating" => "a.rating",
        "kind" => "a.kind",
        "favorite" => "a.is_favorite",
        _ => "a.modified_at",
    };
    let sort_dir = if payload.query.sort.order == "asc" {
        "ASC"
    } else {
        "DESC"
    };

    let count_sql = format!("SELECT COUNT(*) FROM assets a {}", where_clause);
    let data_sql = format!(
        r#"SELECT
            a.id, a.file_path, a.file_name, a.extension, a.kind,
            a.size_bytes, a.modified_at, a.indexed_at, a.rating,
            a.color_label, a.is_favorite, a.description, a.thumbnail_path,
            a.preview_status, a.metadata,
            (SELECT json_group_array(tag_id) FROM asset_tags WHERE asset_id = a.id) AS tags,
            (SELECT json_group_array(collection_id) FROM collection_assets WHERE asset_id = a.id) AS collections
           FROM assets a
           {}
           ORDER BY {} {}
           LIMIT ? OFFSET ?"#,
        where_clause, sort_col, sort_dir
    );

    if target_libs.len() == 1 {
        let (pool, root_opt) = &target_libs[0];

        let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);
        for b in &binds {
            count_query = count_query.bind(b.clone());
        }
        let total = count_query
            .fetch_one(pool)
            .await
            .map_err(|e| e.to_string())?;

        let mut data_query = sqlx::query_as::<_, RawAssetRow>(&data_sql);
        for b in &binds {
            data_query = data_query.bind(b.clone());
        }
        data_query = data_query.bind(limit).bind(offset);

        let rows = data_query
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())?;
        let root_ref = root_opt.as_deref();
        let assets = rows
            .into_iter()
            .map(|r| row_to_asset(root_ref, r))
            .collect();

        return Ok(SearchResult {
            assets,
            total,
            page: payload.page,
            page_size: payload.page_size,
        });
    }

    // Multiple libraries: query each, combine counts, and merge-sort
    let fetch_limit = offset + limit;
    let multi_data_sql = format!(
        r#"SELECT
            a.id, a.file_path, a.file_name, a.extension, a.kind,
            a.size_bytes, a.modified_at, a.indexed_at, a.rating,
            a.color_label, a.is_favorite, a.description, a.thumbnail_path,
            a.preview_status, a.metadata,
            (SELECT json_group_array(tag_id) FROM asset_tags WHERE asset_id = a.id) AS tags,
            (SELECT json_group_array(collection_id) FROM collection_assets WHERE asset_id = a.id) AS collections
           FROM assets a
           {}
           ORDER BY {} {}
           LIMIT ?"#,
        where_clause, sort_col, sort_dir
    );

    let mut total: i64 = 0;
    let mut all_assets: Vec<Asset> = Vec::new();

    for (pool, root_opt) in target_libs {
        let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);
        for b in &binds {
            count_query = count_query.bind(b.clone());
        }
        if let Ok(c) = count_query.fetch_one(pool).await {
            total += c;
        }

        let mut data_query = sqlx::query_as::<_, RawAssetRow>(&multi_data_sql);
        for b in &binds {
            data_query = data_query.bind(b.clone());
        }
        data_query = data_query.bind(fetch_limit);

        if let Ok(rows) = data_query.fetch_all(pool).await {
            let root_ref = root_opt.as_deref();
            for r in rows {
                all_assets.push(row_to_asset(root_ref, r));
            }
        }
    }

    let sort_field = payload.query.sort.field.as_str();
    let is_asc = payload.query.sort.order == "asc";
    all_assets.sort_by(|a, b| {
        let cmp = match sort_field {
            "name" => a.file_name.to_lowercase().cmp(&b.file_name.to_lowercase()),
            "size" => a.size_bytes.cmp(&b.size_bytes),
            "rating" => a.rating.cmp(&b.rating),
            "kind" => a.kind.cmp(&b.kind),
            "favorite" => a.is_favorite.cmp(&b.is_favorite),
            _ => a.modified_at.cmp(&b.modified_at),
        };
        if is_asc {
            cmp
        } else {
            cmp.reverse()
        }
    });

    let paged_assets: Vec<Asset> = all_assets
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();

    Ok(SearchResult {
        assets: paged_assets,
        total,
        page: payload.page,
        page_size: payload.page_size,
    })
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
        .ok_or_else(|| format!("Asset not found: {}", id))?;
    let pool = &lib.pool;

    if let Some(rating) = patch.rating {
        let _ = sqlx::query("UPDATE assets SET rating = ? WHERE id = ?")
            .bind(rating)
            .bind(id)
            .execute(pool)
            .await;
    }
    if let Some(ref color_label) = patch.color_label {
        let _ = sqlx::query("UPDATE assets SET color_label = ? WHERE id = ?")
            .bind(color_label)
            .bind(id)
            .execute(pool)
            .await;
    }
    if let Some(is_fav) = patch.is_favorite {
        let fav_int: i64 = if is_fav { 1 } else { 0 };
        let _ = sqlx::query("UPDATE assets SET is_favorite = ? WHERE id = ?")
            .bind(fav_int)
            .bind(id)
            .execute(pool)
            .await;
    }
    if let Some(ref description) = patch.description {
        let _ = sqlx::query("UPDATE assets SET description = ? WHERE id = ?")
            .bind(description)
            .bind(id)
            .execute(pool)
            .await;
    }
    if let Some(ref thumbnail_path) = patch.thumbnail_path {
        let rel_thumb = make_relative_path(&lib.root_path, Path::new(thumbnail_path));
        let _ = sqlx::query("UPDATE assets SET thumbnail_path = ? WHERE id = ?")
            .bind(rel_thumb)
            .bind(id)
            .execute(pool)
            .await;
    }
    if let Some(ref preview_status) = patch.preview_status {
        let _ = sqlx::query("UPDATE assets SET preview_status = ? WHERE id = ?")
            .bind(preview_status)
            .bind(id)
            .execute(pool)
            .await;
    }
    if let Some(ref tags) = patch.tags {
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        sqlx::query("DELETE FROM asset_tags WHERE asset_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        for tag_id in tags {
            let _ =
                sqlx::query("INSERT OR IGNORE INTO asset_tags (asset_id, tag_id) VALUES (?, ?)")
                    .bind(id)
                    .bind(tag_id)
                    .execute(&mut *tx)
                    .await;
        }
        tx.commit().await.map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[tauri::command]
pub async fn delete_asset(
    id: String,
    delete_file: Option<bool>,
    state: State<'_, Arc<AppState>>,
) -> Result<()> {
    let should_delete_file = delete_file.unwrap_or(true);
    let libs = state.db.get_all_libraries().await;
    let mut target_pools = Vec::new();
    let mut file_to_delete: Option<std::path::PathBuf> = None;

    for lib in &libs {
        let row: Option<(String,)> = sqlx::query_as("SELECT file_path FROM assets WHERE id = ?")
            .bind(&id)
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
        }
    }

    target_pools.push(state.db.master_pool.clone());

    // If deleting from filesystem
    if should_delete_file {
        if let Some(ref path) = file_to_delete {
            if path.exists() {
                // Try moving to Recycle Bin first
                if let Err(trash_err) = trash::delete(path) {
                    tracing::warn!(
                        "Failed to trash {:?}, falling back to remove_file: {}",
                        path,
                        trash_err
                    );
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }

    // Delete records from databases
    for pool in target_pools {
        let _ = sqlx::query("DELETE FROM asset_tags WHERE asset_id = ?")
            .bind(&id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM collection_assets WHERE asset_id = ?")
            .bind(&id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM preview_jobs WHERE asset_id = ?")
            .bind(&id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM assets WHERE id = ?")
            .bind(&id)
            .execute(&pool)
            .await;
        let _ = sqlx::query("UPDATE libraries SET asset_count = (SELECT COUNT(*) FROM assets)")
            .execute(&pool)
            .await;
    }

    let _ = state.app_handle.emit("library:updated", ());
    Ok(())
}

#[tauri::command]
pub async fn delete_assets(
    ids: Vec<String>,
    delete_file: Option<bool>,
    state: State<'_, Arc<AppState>>,
) -> Result<()> {
    for id in ids {
        let _ = delete_asset(id, delete_file, state.clone()).await;
    }
    Ok(())
}

#[tauri::command]
pub async fn rename_asset(
    id: String,
    new_name: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Asset> {
    let trimmed = new_name.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return Err("Asset file name cannot be empty, '.' or '..'".into());
    }

    if trimmed.contains(['/', '\0']) {
        return Err("File name cannot contain '/' or NUL".into());
    }

    let libs = state.db.get_all_libraries().await;
    let mut found_lib = None;
    let mut current_fp = None;
    let mut current_ext = None;

    for lib in &libs {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT file_path, extension FROM assets WHERE id = ?")
                .bind(&id)
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
    if old_abs_path.exists() {
        std::fs::rename(&old_abs_path, &new_abs_path)
            .map_err(|e| format!("Failed to rename file on disk: {}", e))?;
    }

    let new_rel_fp = make_relative_path(&lib.root_path, &new_abs_path);
    let now = chrono::Utc::now().timestamp_millis();

    // Update in all relevant databases
    let pools_to_update = vec![lib.pool.clone(), state.db.master_pool.clone()];
    for pool in pools_to_update {
        let _ = sqlx::query(
            "UPDATE assets SET file_name = ?, file_path = ?, extension = ?, modified_at = ? WHERE id = ?"
        )
        .bind(&final_name)
        .bind(&new_rel_fp)
        .bind(&new_ext)
        .bind(now)
        .bind(&id)
        .execute(&pool)
        .await;
    }

    let _ = state.app_handle.emit("library:updated", ());
    get_asset(id, state).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        insert_asset, insert_tag, memory_database, memory_library, memory_library_pool,
    };

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
}
