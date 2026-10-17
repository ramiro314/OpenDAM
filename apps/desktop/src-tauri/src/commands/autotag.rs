use std::sync::Arc;
use tauri::State;

use crate::autotag::engine::{ProviderOption, RunScope, StartedRun};
use crate::autotag::provider::Mode;
use crate::autotag::review::{ProposalDecision, ReviewProposal};
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

/// Starts a background run over chosen assets, or over a library's untagged assets (every
/// asset with `allAssets`). Progress arrives as `autotag:updated` events.
#[tauri::command]
pub async fn start_autotag_run(
    scope: RunScope,
    provider: String,
    mode: Mode,
    state: State<'_, Arc<AppState>>,
) -> Result<StartedRun> {
    state.autotag.start_run(scope, &provider, mode).await
}

/// Every run, newest first.
#[tauri::command]
pub async fn list_autotag_runs(state: State<'_, Arc<AppState>>) -> Result<Vec<RunSummary>> {
    state.autotag.list_runs().await.map_err(|e| e.to_string())
}

/// Stops handing out the run's items; requests in flight finish.
#[tauri::command]
pub async fn pause_autotag_run(run_id: String, state: State<'_, Arc<AppState>>) -> Result<()> {
    state.autotag.pause_run(&run_id).await
}

/// Resumes a paused run. Fails, leaving it paused, when its provider isn't ready.
#[tauri::command]
pub async fn resume_autotag_run(run_id: String, state: State<'_, Arc<AppState>>) -> Result<()> {
    state.autotag.resume_run(&run_id).await
}

/// Resumes every paused run whose provider is ready, and returns how many.
#[tauri::command]
pub async fn resume_all_autotag_runs(state: State<'_, Arc<AppState>>) -> Result<usize> {
    state.autotag.resume_all().await
}

/// Cancels a run: its remaining items are skipped, and the tags it applied stay.
#[tauri::command]
pub async fn cancel_autotag_run(run_id: String, state: State<'_, Arc<AppState>>) -> Result<()> {
    state.autotag.cancel_run(&run_id).await
}

/// The run's new tag names waiting for review, each with its assets.
#[tauri::command]
pub async fn get_autotag_proposals(
    run_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<ReviewProposal>> {
    state.autotag.proposals(&run_id).await
}

/// Applies the review: accepted tags are created (master, then every open library), the
/// ticked assets tagged, and both recorded for undo. Returns the run's status: `done` once
/// nothing is left to review, or `cancelled` for a cancelled run.
#[tauri::command]
pub async fn review_autotag_proposals(
    run_id: String,
    decisions: Vec<ProposalDecision>,
    state: State<'_, Arc<AppState>>,
) -> Result<String> {
    state.autotag.review_proposals(&run_id, decisions).await
}
