use std::sync::Arc;
use tauri::State;

use crate::import::engine::ImportRequest;
use crate::import::records::RunSummary;
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
