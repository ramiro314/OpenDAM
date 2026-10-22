use std::sync::Arc;
use tauri::State;

use crate::export::settings::{self, ExportSettings};
use crate::export::{cache, check};
use crate::AppState;

type Result<T> = std::result::Result<T, String>;

#[tauri::command]
pub async fn get_export_settings(state: State<'_, Arc<AppState>>) -> Result<ExportSettings> {
    settings::load(&state.db.master_pool)
        .await
        .map_err(|e| e.to_string())
}

/// `patch` is a partial `ExportSettings`, for example `{"root": "/home/me/export"}`, where null
/// clears a field. An export root in or around a library is refused.
#[tauri::command]
pub async fn set_export_settings(
    patch: serde_json::Value,
    state: State<'_, Arc<AppState>>,
) -> Result<ExportSettings> {
    settings::update(&state.db.master_pool, patch).await
}

/// Deletes every open library's `.opendam/export-cache/`, returning how many it deleted.
#[tauri::command]
pub async fn clear_export_cache(state: State<'_, Arc<AppState>>) -> Result<usize> {
    cache::clear(&state.db.get_all_libraries().await).await
}

/// Checks the domain collection `domain_id` for export (plan C14): every asset with its kind,
/// default name, pack and status, every problem that refuses the export, and the manifest it
/// would write.
#[tauri::command]
pub async fn check_export(
    domain_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<check::CheckReport> {
    check::check(&state.db, &domain_id).await
}
