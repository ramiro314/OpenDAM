use std::sync::Arc;
use tauri::State;

use crate::import::engine::ImportRequest;
use crate::import::place::PlaceStep;
use crate::import::records::{ImportSummary, PackSteps, RunSummary, Step};
use crate::import::scan::{Scan, Tree};
use crate::import::settings::{self, ImportSettings};
use crate::AppState;

type Result<T> = std::result::Result<T, String>;

/// Scans a `.unitypackage` or `.zip` original for an import into a library: its candidate
/// packs, each with its preset, default target (with the reason the library refuses it, if it
/// does), default steps and the folder tree its build would produce, with per-folder file counts
/// and the GLB count. Nothing is written.
#[tauri::command]
pub async fn scan_import(
    library_id: String,
    original_path: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Scan> {
    state.imports.scan(&library_id, &original_path).await
}

/// The folder tree one source of an original would build with the place steps `place`, for the
/// import dialog once a candidate's steps change. Nothing is written.
#[tauri::command]
pub async fn preview_import(
    library_id: String,
    original_path: String,
    variant: Option<String>,
    subpath: Option<String>,
    place: Vec<PlaceStep>,
    state: State<'_, Arc<AppState>>,
) -> Result<Tree> {
    state
        .imports
        .preview_import(
            &library_id,
            &original_path,
            variant.as_deref(),
            subpath.as_deref(),
            place,
        )
        .await
}

/// Why the library would refuse `target` for a new pack, or with `pack_id` as that pack's new
/// target (it exists, or nests with another pack's target, naming it), or `None` when it is free.
#[tauri::command]
pub async fn check_import_target(
    library_id: String,
    target: String,
    pack_id: Option<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<Option<String>> {
    state
        .imports
        .check_import_target(&library_id, &target, pack_id.as_deref())
        .await
}

/// Moves a pack's target, refusing one that exists in the library or nests with another pack's
/// target, and any move of a built pack or one with a run queued or running.
#[tauri::command]
pub async fn set_import_target(
    library_id: String,
    pack_id: String,
    target: String,
    state: State<'_, Arc<AppState>>,
) -> Result<()> {
    state
        .imports
        .set_target(&library_id, &pack_id, &target)
        .await
}

/// Imports a `.unitypackage` or `.zip` original as a new pack from an explicit source in it (a
/// zip's variant `.unitypackage` or folder, a package's pipeline folder), at an explicit target
/// folder, with its publisher, and queues its first build. Progress arrives as `import:progress` events, status
/// changes as `import:updated`, and `library:updated` once the build changed the library.
#[tauri::command]
pub async fn start_import(
    request: ImportRequest,
    state: State<'_, Arc<AppState>>,
) -> Result<RunSummary> {
    state.imports.start_import(request).await
}

/// Cancels a queued run, or the running one until its build takes the library's lock. After
/// that it fails with "Finishing, can't cancel now".
#[tauri::command]
pub async fn cancel_import_run(
    library_id: String,
    run_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<()> {
    state.imports.cancel_run(&library_id, &run_id).await
}

/// A library's imports, newest first, each with its original and its packs.
#[tauri::command]
pub async fn list_imports(
    library_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<ImportSummary>> {
    state.imports.list_imports(&library_id).await
}

/// A library's import runs, newest first.
#[tauri::command]
pub async fn list_import_runs(
    library_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<RunSummary>> {
    state.imports.list_runs(&library_id).await
}

/// An imported pack's steps in order, with its `recipe_rev`.
#[tauri::command]
pub async fn get_import_steps(
    library_id: String,
    pack_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<PackSteps> {
    state.imports.pack_steps(&library_id, &pack_id).await
}

/// Replaces an imported pack's steps (turn one off or on, change its options) and returns its
/// new `recipe_rev`. Refused while a run for the pack is queued or running, and, given the
/// `recipe_rev` the steps were read at, once they changed since. The pack changes on disk only
/// when it is rebuilt.
#[tauri::command]
pub async fn set_import_steps(
    library_id: String,
    pack_id: String,
    steps: Vec<Step>,
    recipe_rev: Option<i64>,
    state: State<'_, Arc<AppState>>,
) -> Result<i64> {
    state
        .imports
        .set_pack_steps(&library_id, &pack_id, steps, recipe_rev)
        .await
}

/// Forgets an imported Kenney pack's store page answer, so its next build looks the page up
/// again with HEAD requests to kenney.nl. Refused while a run for the pack is queued or running.
#[tauri::command]
pub async fn clear_import_store_page(
    library_id: String,
    pack_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<()> {
    state.imports.clear_store_page(&library_id, &pack_id).await
}

/// Queues a rebuild of an imported pack from its current steps. Its new output replaces the pack
/// folder only once fully built and indexed, and every asset row and its metadata is kept.
/// Refused while a run for the pack is queued or running.
#[tauri::command]
pub async fn rebuild_import_pack(
    library_id: String,
    pack_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<RunSummary> {
    state.imports.rebuild_pack(&library_id, &pack_id).await
}

/// The import settings: Blender's memory cap for the `unity_glb` step.
#[tauri::command]
pub async fn get_import_settings(state: State<'_, Arc<AppState>>) -> Result<ImportSettings> {
    settings::load(&state.db.master_pool)
        .await
        .map_err(|e| e.to_string())
}

/// `patch` is a partial `ImportSettings`, such as `{"blenderMemoryMax": "16G"}`; null clears a
/// field. A cap systemd can't read saves nothing.
#[tauri::command]
pub async fn set_import_settings(
    patch: serde_json::Value,
    state: State<'_, Arc<AppState>>,
) -> Result<ImportSettings> {
    settings::update(&state.db.master_pool, patch).await
}
