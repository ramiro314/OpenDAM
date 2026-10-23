pub mod autotag;
pub mod commands;
pub mod db;
pub mod export;
pub mod model_preview_regen;
pub mod models;
pub mod preview;
pub mod render;
pub mod structure;
pub mod thumb_cleanup;
pub mod watcher;

use std::sync::Arc;
use tauri::{Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use tracing_subscriber::EnvFilter;

use autotag::anthropic::AnthropicProvider;
use autotag::claude_cli::ClaudeCli;
use autotag::credentials::Credentials;
use autotag::engine::Engine;
use autotag::jev::JevProvider;
use autotag::provider::TagProvider;
use db::Database;

pub struct AppState {
    pub db: Arc<Database>,
    pub credentials: Arc<Credentials>,
    pub anthropic: Arc<AnthropicProvider>,
    /// Claude-plan access, which `anthropic` also runs through. Personal use only.
    pub claude: Arc<ClaudeCli>,
    pub jev: Arc<JevProvider>,
    pub autotag: Arc<Engine>,
    pub app_handle: tauri::AppHandle,
    /// The preview engine version the Python queue's worker process reported to `engine_info`,
    /// or `None` until a process answers (`preview::PreviewEngine`).
    pub engine_version: tokio::sync::watch::Sender<Option<i64>>,
    /// The `worker.py` the Python queue runs, once `preview::start_preview_worker` has found it.
    pub preview_worker: std::sync::OnceLock<std::path::PathBuf>,
    /// The game export's staged bundles, one per open Export view
    pub exports: crate::export::bundle::Exports,
    /// FBX facts for the game export, from the Python queue's worker, cached for the session
    pub fbx_facts: export::fbx_facts::Cached<export::fbx_facts::Worker>,
}

/// The tagging providers runs can use, plus the fake ones in a debug build run with
/// `OPENDAM_AUTOTAG_FAKE=1`.
fn tag_providers(
    anthropic: &Arc<AnthropicProvider>,
    jev: &Arc<JevProvider>,
) -> Vec<Arc<dyn TagProvider>> {
    #[allow(unused_mut)]
    let mut providers: Vec<Arc<dyn TagProvider>> = vec![anthropic.clone(), jev.clone()];
    #[cfg(debug_assertions)]
    providers.extend(
        autotag::fake::from_env()
            .into_iter()
            .map(|p| Arc::new(p) as Arc<dyn TagProvider>),
    );
    providers
}

/// Runs the app. `render_choice` is what `main.rs` did to the rendering environment, logged
/// once tracing is up.
pub fn run(render_choice: render::RenderChoice) {
    // Logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "opendam=debug,info".parse().unwrap()),
        )
        .init();
    tracing::info!(
        target: "render",
        choice = %render_choice,
        mode = render::current_render_mode(),
        "WebKitGTK rendering mode"
    );

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let app_data_dir = app
                .path()
                .app_data_dir()
                .expect("Failed to get app data directory");

            std::fs::create_dir_all(&app_data_dir)?;

            let db_path = app_data_dir.join("opendam.sqlite");

            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            let (db, libraries) = rt.block_on(async {
                let db = Database::new(db_path.to_str().unwrap())
                    .await
                    .expect("Failed to initialize database");
                #[cfg(debug_assertions)]
                commands::bench::register_bench_library(
                    &db,
                    std::env::var("OPENDAM_BENCH_LIBRARY").ok(),
                )
                .await;
                let libraries =
                    sqlx::query_as::<_, (String, String)>("SELECT id, root_paths FROM libraries")
                        .fetch_all(&db.master_pool)
                        .await;
                (db, libraries)
            });
            let libraries: Vec<(String, Vec<String>)> = match libraries {
                Ok(rows) => rows
                    .into_iter()
                    .map(|(id, root_paths_json)| {
                        (
                            id,
                            serde_json::from_str(&root_paths_json).unwrap_or_default(),
                        )
                    })
                    .collect(),
                Err(e) => {
                    tracing::error!("Failed to list libraries: {}", e);
                    Vec::new()
                }
            };

            // Before the webview asks for any thumbnail
            for (_, paths) in &libraries {
                commands::library::allow_library_roots(app.handle(), paths);
            }

            let db = Arc::new(db);
            let credentials = Arc::new(Credentials::from_process_env());
            let claude = Arc::new(ClaudeCli::from_process_env(app_data_dir.clone()));
            let anthropic = Arc::new(AnthropicProvider::new(credentials.clone(), claude.clone()));
            let jev = Arc::new(JevProvider::new(credentials.clone()));
            let events = app.handle().clone();
            let notifications = app.handle().clone();
            let autotag = Engine::new(
                db.clone(),
                tag_providers(&anthropic, &jev),
                Arc::new(move |update| {
                    let _ = events.emit("autotag:updated", update);
                }),
                Arc::new(move |notice| {
                    let shown = notifications
                        .notification()
                        .builder()
                        .title(notice.title)
                        .body(notice.body)
                        .show();
                    if let Err(e) = shown {
                        tracing::warn!("Could not show a desktop notification: {e}");
                    }
                }),
            );
            let (worker_requests, requests) = tokio::sync::mpsc::channel(64);
            let state = Arc::new(AppState {
                db,
                credentials,
                anthropic,
                claude,
                jev,
                autotag: autotag.clone(),
                app_handle: app.handle().clone(),
                engine_version: tokio::sync::watch::Sender::new(None),
                preview_worker: std::sync::OnceLock::new(),
                exports: Default::default(),
                fbx_facts: export::fbx_facts::Cached::new(export::fbx_facts::Worker::new(
                    worker_requests,
                )),
            });

            app.manage(state.clone());

            // Start the background preview worker queue
            crate::preview::start_preview_worker(state.clone(), requests);

            // Removes upscaled thumbnails once per library; `Database::new` already synced them
            thumb_cleanup::spawn(state.clone(), None);

            // Requeues FBX/3DS previews an older engine made, now and whenever the queue's
            // worker declares its version
            model_preview_regen::start(state.clone());

            // Auto-tagging runs: replays writes left pending, then serves queued items
            tauri::async_runtime::spawn(async move { autotag.start().await });

            // Re-index all existing libraries on startup in background
            let startup_state = state.clone();
            tauri::async_runtime::spawn(async move {
                for (id, paths) in libraries {
                    if let Err(e) = watcher::index_paths(&startup_state.db, &id, &paths).await {
                        tracing::error!("Failed to index library {}: {}", id, e);
                    }
                }
                let _ = startup_state.app_handle.emit("library:updated", ());
            });

            // ── Cross-machine NAS change detection ───────────────────────────────
            // When another machine writes to the shared library.sqlite, its mtime
            // changes on the NAS. We poll every 30s and emit library:updated when
            // we detect a newer mtime, causing the frontend to re-query automatically.
            let poll_state = state.clone();
            tauri::async_runtime::spawn(async move {
                use std::collections::HashMap;
                use std::time::SystemTime;

                // seed: record the initial mtime for each known library DB
                let mut last_seen: HashMap<String, SystemTime> = HashMap::new();

                loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

                    let libs = poll_state.db.get_all_libraries().await;
                    let mut changed = false;

                    for lib in &libs {
                        let db_path = lib.root_path.join(".opendam").join("library.sqlite");
                        if let Ok(meta) = std::fs::metadata(&db_path) {
                            if let Ok(mtime) = meta.modified() {
                                let prev = last_seen.entry(lib.id.clone()).or_insert(mtime);
                                if mtime > *prev {
                                    *prev = mtime;
                                    changed = true;
                                    tracing::debug!(
                                        "NAS poll: library '{}' DB updated by another machine",
                                        lib.name
                                    );
                                }
                            }
                        }
                    }

                    if changed {
                        let _ = poll_state.app_handle.emit("library:updated", ());
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::library::add_library,
            commands::library::list_libraries,
            commands::library::remove_library,
            commands::library::refresh_library,
            commands::library::refresh_all_libraries,
            commands::asset::search_assets,
            commands::asset::get_facet_counts,
            commands::asset::get_folder_counts,
            commands::asset::get_asset,
            commands::asset::get_group_members,
            commands::asset::update_asset,
            commands::asset::delete_asset,
            commands::asset::delete_assets,
            commands::asset::rename_asset,
            commands::folder::get_folders,
            commands::folder::get_packs,
            commands::folder::get_pack_detail,
            commands::folder::set_pack_override,
            commands::folder::get_cover_pack,
            commands::folder::tag_folder,
            commands::folder::collect_folder,
            commands::tag::list_tags,
            commands::tag::create_tag,
            commands::tag::update_tag,
            commands::tag::delete_tag,
            commands::collection::list_collections,
            commands::collection::create_collection,
            commands::collection::add_to_collection,
            commands::collection::remove_from_collection,
            commands::collection::delete_collection,
            commands::preview::request_preview,
            commands::preview::get_preview_jobs,
            commands::preview::list_pending_model_thumbnails,
            commands::preview::save_model_thumbnail,
            commands::preview::get_preview_progress,
            commands::shell::reveal_in_file_manager,
            commands::shell::open_with_default,
            commands::shell::open_url,
            commands::settings::get_ai_settings,
            commands::settings::set_ai_settings,
            commands::settings::set_api_key,
            commands::settings::get_provider_status,
            commands::settings::test_provider,
            commands::settings::get_claude_plan_status,
            commands::export::get_export_settings,
            commands::export::set_export_settings,
            commands::export::clear_export_cache,
            commands::export::get_blender_status,
            commands::export::check_export,
            commands::export::export_bundle,
            commands::export::discard_export,
            commands::export::get_inferred_pack_origin,
            commands::autotag::list_autotag_providers,
            commands::autotag::estimate_autotag_run,
            commands::autotag::start_autotag_run,
            commands::autotag::get_autotag_last_used,
            commands::autotag::get_autotag_suggestions,
            commands::autotag::list_autotag_runs,
            commands::autotag::pause_autotag_run,
            commands::autotag::resume_autotag_run,
            commands::autotag::resume_all_autotag_runs,
            commands::autotag::cancel_autotag_run,
            commands::autotag::get_autotag_proposals,
            commands::autotag::review_autotag_proposals,
            commands::autotag::undo_autotag_run,
            commands::autotag::retry_failed_autotag_run,
            commands::autotag::clear_autotag_runs,
            #[cfg(debug_assertions)]
            commands::bench::bench_clear_thumbnails,
            #[cfg(debug_assertions)]
            commands::bench::bench_rss,
            #[cfg(debug_assertions)]
            commands::bench::bench_render_mode,
            #[cfg(debug_assertions)]
            commands::bench::bench_log,
            #[cfg(debug_assertions)]
            commands::bench::bench_only,
            #[cfg(debug_assertions)]
            commands::bench::bench_report,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
