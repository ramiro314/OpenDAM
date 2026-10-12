use std::sync::Arc;
use serde::Serialize;
use tauri::State;
use tauri::ipc::InvokeBody;
use uuid::Uuid;
use chrono::Utc;

use crate::AppState;
use crate::models::PreviewJob;
use crate::db::resolve_path;

type Result<T> = std::result::Result<T, String>;

#[tauri::command]
pub async fn request_preview(
    asset_ids: Vec<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<()> {
    let libs = state.db.get_all_libraries().await;
    let now = Utc::now().timestamp_millis();

    for asset_id in &asset_ids {
        let mut target_pool = None;
        for lib in &libs {
            let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM assets WHERE id = ?")
                .bind(asset_id)
                .fetch_optional(&lib.pool)
                .await
                .unwrap_or(None);

            if exists.is_some() {
                target_pool = Some(lib.pool.clone());
                break;
            }
        }

        let pool = match target_pool {
            Some(p) => p,
            None => state.db.get_active_pool().await,
        };

        // Delete any existing preview job so a new one is queued cleanly
        let _ = sqlx::query("DELETE FROM preview_jobs WHERE asset_id = ?")
            .bind(asset_id)
            .execute(&pool)
            .await;

        let id = Uuid::new_v4().to_string();
        let _ = sqlx::query(
            "INSERT INTO preview_jobs (id, asset_id, status, created_at) VALUES (?, ?, 'queued', ?)"
        )
        .bind(&id).bind(asset_id).bind(now)
        .execute(&pool).await;

        // Mark asset as generating / pending
        let _ = sqlx::query("UPDATE assets SET preview_status = 'pending' WHERE id = ?")
            .bind(asset_id).execute(&pool).await;
    }
    Ok(())
}

#[tauri::command]
pub async fn get_preview_jobs(state: State<'_, Arc<AppState>>) -> Result<Vec<PreviewJob>> {
    let libs = state.db.get_all_libraries().await;
    let mut all_jobs = Vec::new();

    for lib in &libs {
        if let Ok(rows) = sqlx::query_as::<_, (String, String, String, Option<i64>, Option<String>, i64)>(
            "SELECT id, asset_id, status, progress, error_message, created_at FROM preview_jobs ORDER BY created_at DESC LIMIT 50"
        )
        .fetch_all(&lib.pool)
        .await {
            for (id, asset_id, status, progress, error_message, created_at) in rows {
                all_jobs.push(PreviewJob { id, asset_id, status, progress, error_message, created_at });
            }
        }
    }

    all_jobs.sort_by_key(|b| std::cmp::Reverse(b.created_at));
    all_jobs.truncate(100);
    Ok(all_jobs)
}

/// A converted 3D model whose thumbnail is still the .glb itself, waiting to be rendered by the frontend.
#[derive(Serialize)]
pub struct PendingModelThumbnail {
    #[serde(rename = "assetId")]
    pub asset_id: String,
    #[serde(rename = "modelPath")]
    pub model_path: String,
}

#[tauri::command]
pub async fn list_pending_model_thumbnails(
    limit: i64,
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<PendingModelThumbnail>> {
    let mut pending = Vec::new();
    for lib in state.db.get_all_libraries().await {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, thumbnail_path FROM assets WHERE thumbnail_path LIKE '%.glb' LIMIT ?"
        )
        .bind(limit - pending.len() as i64)
        .fetch_all(&lib.pool)
        .await
        .map_err(|e| e.to_string())?;

        pending.extend(rows.into_iter().map(|(asset_id, thumb)| PendingModelThumbnail {
            asset_id,
            model_path: resolve_path(&lib.root_path, &thumb),
        }));
        if pending.len() as i64 >= limit {
            break;
        }
    }
    Ok(pending)
}

/// Stores a PNG thumbnail rendered by the frontend for a 3D model.
/// Takes the raw PNG as the request body and the asset id in an `asset-id` header.
/// Returns the absolute thumbnail path.
#[tauri::command]
pub async fn save_model_thumbnail(
    request: tauri::ipc::Request<'_>,
    state: State<'_, Arc<AppState>>,
) -> Result<String> {
    let asset_id = request
        .headers()
        .get("asset-id")
        .and_then(|v| v.to_str().ok())
        .ok_or("missing asset-id header")?;
    let png = match request.body() {
        InvokeBody::Raw(png) if !png.is_empty() => png,
        _ => return Err("expected a raw PNG body".to_string()),
    };

    for lib in state.db.get_all_libraries().await {
        let exists: Option<(String,)> = sqlx::query_as("SELECT id FROM assets WHERE id = ?")
            .bind(asset_id)
            .fetch_optional(&lib.pool)
            .await
            .map_err(|e| e.to_string())?;
        if exists.is_none() {
            continue;
        }

        // The id was just matched against the DB, so it's safe to use as a file name.
        let rel_thumb = format!(".opendam/thumbs/{}_thumb.png", asset_id);
        let abs_thumb = resolve_path(&lib.root_path, &rel_thumb);
        tokio::fs::write(&abs_thumb, png).await.map_err(|e| e.to_string())?;
        sqlx::query("UPDATE assets SET thumbnail_path = ? WHERE id = ?")
            .bind(&rel_thumb)
            .bind(asset_id)
            .execute(&lib.pool)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(abs_thumb);
    }
    Err(format!("asset {} not found", asset_id))
}

