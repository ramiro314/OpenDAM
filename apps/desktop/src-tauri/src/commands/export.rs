use serde::Serialize;
use std::sync::Arc;
use tauri::{Emitter, State};

use crate::export::blender::{self, BlenderStatus};
use crate::export::bundle::ExportOutcome;
use crate::export::game::{self, GameDetail};
use crate::export::packs::Origin;
use crate::export::settings::{self, ExportSettings};
use crate::export::{cache, check, edits};
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
/// write and the diff against the previous bundle. FBX files are read through the preview
/// worker's `fbx_facts`, and each model the export cache doesn't hold is normalized by headless
/// Blender (C9), with an `export:progress` event before the jobs start and as each ends. With no
/// problem, it stages the bundle beside the domain's folder for `export_bundle` (C15).
#[tauri::command]
pub async fn check_export(
    domain_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<check::CheckReport> {
    let app = state.app_handle.clone();
    let progress = |progress: check::Progress| {
        let _ = app.emit(
            "export:progress",
            ExportProgress {
                domain_id: &domain_id,
                progress,
            },
        );
    };
    state
        .exports
        .check(&state.db, &domain_id, &state.fbx_facts, &progress)
        .await
}

/// An `export:progress` event: how the Blender jobs of a Check of `domain_id` are going.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ExportProgress<'a> {
    domain_id: &'a str,
    #[serde(flatten)]
    progress: check::Progress,
}

/// Swaps the bundle the last Check of `domain_id` staged into the domain's folder, unless
/// anything that Check read has changed since, which asks for a new Check instead.
#[tauri::command]
pub async fn export_bundle(
    domain_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<ExportOutcome> {
    state
        .exports
        .export(&state.db, &domain_id, &state.fbx_facts)
        .await
}

/// Deletes the bundle the last Check of `domain_id` staged, as its Export view closes.
#[tauri::command]
pub async fn discard_export(domain_id: String, state: State<'_, Arc<AppState>>) -> Result<()> {
    state.exports.discard(&domain_id).await
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

/// What the detail panel's Game tab shows for asset `asset_id` (plan C13): what it exports as,
/// its `assetName` inferred and yours, and the problems naming it in the Checks of the domains
/// holding it. Null when it exports as neither a model, an animation nor a texture.
#[tauri::command]
pub async fn get_game_detail(
    asset_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Option<GameDetail>> {
    game::detail(&state.db, &asset_id, &state.fbx_facts, state.exports.memo()).await
}

/// Writes your export edit `patch` to asset `asset_id` (plan C4), for example
/// `{"assetName": "Crate"}`, where null resets a value to inferred. It reaches every member of
/// the asset's group and, for an image, every source of its texture set. Returns the ids it
/// wrote, so an Export view whose Check holds one asks for a new Check.
#[tauri::command]
pub async fn set_export_edit(
    asset_id: String,
    patch: edits::Edits,
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<String>> {
    let library = state
        .db
        .library_of_asset(&asset_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("No asset has the id {asset_id}"))?;
    edits::write(&library.pool, &asset_id, &patch).await
}

/// Shows model `asset_id` as the next export writes it (plan C13): the path of a GLB of its
/// normalized FBX, with your facing, which Blender writes beside its export cache entry when the
/// cache holds none. Blender runs only here, when you ask, and an error says why it can't.
#[tauri::command]
pub async fn show_as_exported(asset_id: String, state: State<'_, Arc<AppState>>) -> Result<String> {
    state
        .exports
        .exported_view(&state.db, &asset_id, &state.fbx_facts)
        .await
        .map(|glb| glb.display().to_string())
}
