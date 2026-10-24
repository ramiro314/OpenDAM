use std::sync::Arc;
use tauri::State;

use crate::import::engine::ImportRequest;
use crate::import::records::{PackSteps, RunSummary, Step};
use crate::AppState;

type Result<T> = std::result::Result<T, String>;

/// Imports a `.unitypackage` original as a new pack at an explicit target folder, with its
/// publisher, and queues its first build. Progress arrives as `import:progress` events, status
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
/// new `recipe_rev`. Refused while a run for the pack is queued or running. The pack changes on
/// disk only when it is rebuilt.
#[tauri::command]
pub async fn set_import_steps(
    library_id: String,
    pack_id: String,
    steps: Vec<Step>,
    state: State<'_, Arc<AppState>>,
) -> Result<i64> {
    state
        .imports
        .set_pack_steps(&library_id, &pack_id, steps)
        .await
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
