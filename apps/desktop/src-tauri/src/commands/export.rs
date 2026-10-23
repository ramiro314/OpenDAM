use std::sync::Arc;
use tauri::State;

use crate::export::blender::{self, BlenderStatus};
use crate::export::packs::Origin;
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
/// default name, pack and status, every problem that refuses the export, and the manifest it
/// would write. FBX files are read through the preview worker's `fbx_facts`.
#[tauri::command]
pub async fn check_export(
    domain_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<check::CheckReport> {
    check::check(&state.db, &domain_id, &state.fbx_facts).await
}

/// The origin the game export infers for the pack at `path` (`""` for the root) in library
/// `library_id`, without your override (plan A1), or null when no pack is there. The pack edit
/// form shows it, and the normal-map convention it implies, beside your choice.
#[tauri::command]
pub async fn get_inferred_pack_origin(
    library_id: String,
    path: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Option<Origin>> {
    let library = state
        .db
        .get_library_handle(&library_id)
        .await
        .ok_or_else(|| format!("Library {library_id} is not open"))?;
    check::inferred_origin(&library, &path).await
}
