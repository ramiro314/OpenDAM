use std::sync::Arc;
use tauri::State;

use crate::autotag::engine::{ProviderOption, StartedRun};
use crate::autotag::provider::Mode;
use crate::autotag::store::RunSummary;
use crate::AppState;

type Result<T> = std::result::Result<T, String>;

/// Every tagging provider, with the modes it supports and why it can't be picked, if it can't.
#[tauri::command]
pub async fn list_autotag_providers(
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<ProviderOption>> {
    let engine = state.autotag.clone();
    // Provider status reads the keyring, which blocks on D-Bus
    tauri::async_runtime::spawn_blocking(move || engine.provider_options())
        .await
        .map_err(|e| e.to_string())
}

/// Starts a background run over `asset_ids`. Progress arrives as `autotag:updated` events.
#[tauri::command]
pub async fn start_autotag_run(
    asset_ids: Vec<String>,
    provider: String,
    mode: Mode,
    state: State<'_, Arc<AppState>>,
) -> Result<StartedRun> {
    state.autotag.start_run(asset_ids, &provider, mode).await
}

/// Every run, newest first.
#[tauri::command]
pub async fn list_autotag_runs(state: State<'_, Arc<AppState>>) -> Result<Vec<RunSummary>> {
    state.autotag.list_runs().await.map_err(|e| e.to_string())
}
