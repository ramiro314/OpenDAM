use std::sync::Arc;
use tauri::State;

use crate::export::blender::{self, BlenderStatus};
use crate::export::bundle::ExportOutcome;
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

/// The Blender an export would run from `path`, Blender's path as typed in Settings (null or
/// empty means `blender` on `PATH`): its version, and why exports can't use it, when they can't.
#[tauri::command]
pub async fn get_blender_status(path: Option<String>) -> Result<BlenderStatus> {
    let path = path.filter(|p| !p.trim().is_empty());
    Ok(blender::status(path.as_deref().map(str::trim)).await)
}

/// Checks the domain collection `domain_id` for export (plan C14): every asset with its kind,
/// default name, pack and status, every problem that refuses the export, the manifest it would
/// write and the diff against the previous bundle. With no problem, it stages the bundle beside
/// the domain's folder for `export_bundle` (C15).
#[tauri::command]
pub async fn check_export(
    domain_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<check::CheckReport> {
    state.exports.check(&state.db, &domain_id).await
}

/// Swaps the bundle the last Check of `domain_id` staged into the domain's folder, unless
/// anything that Check read has changed since, which asks for a new Check instead.
#[tauri::command]
pub async fn export_bundle(
    domain_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<ExportOutcome> {
    state.exports.export(&state.db, &domain_id).await
}

/// Deletes the bundle the last Check of `domain_id` staged, as its Export view closes.
#[tauri::command]
pub async fn discard_export(domain_id: String, state: State<'_, Arc<AppState>>) -> Result<()> {
    state.exports.discard(&domain_id).await
}
