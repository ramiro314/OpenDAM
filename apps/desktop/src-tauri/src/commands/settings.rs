use std::sync::Arc;
use tauri::State;

use crate::autotag::anthropic::ConnectionTest;
use crate::autotag::claude_cli::ClaudePlanStatus;
use crate::autotag::credentials::{Provider, ProviderStatus};
use crate::autotag::settings::{self, AiSettings, AnthropicAccess};
use crate::AppState;

type Result<T> = std::result::Result<T, String>;

#[tauri::command]
pub async fn get_ai_settings(state: State<'_, Arc<AppState>>) -> Result<AiSettings> {
    settings::load(&state.db.master_pool)
        .await
        .map_err(|e| e.to_string())
}

/// `patch` is a partial `AiSettings`, for example `{"jev": {"threshold": 0.8}}`.
#[tauri::command]
pub async fn set_ai_settings(
    patch: serde_json::Value,
    state: State<'_, Arc<AppState>>,
) -> Result<AiSettings> {
    settings::update(&state.db.master_pool, patch).await
}

/// Saves `key` in the keyring, or clears it when `key` is null. Returns the new status, never
/// the key.
#[tauri::command]
pub async fn set_api_key(
    provider: Provider,
    key: Option<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<ProviderStatus> {
    let credentials = state.credentials.clone();
    // Keyring calls block on D-Bus
    tauri::async_runtime::spawn_blocking(move || credentials.set_key(provider, key.as_deref()))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn get_provider_status(state: State<'_, Arc<AppState>>) -> Result<Vec<ProviderStatus>> {
    let credentials = state.credentials.clone();
    tauri::async_runtime::spawn_blocking(move || credentials.statuses())
        .await
        .map_err(|e| e.to_string())
}

/// Claude-plan access as Settings shows it: `claude`'s path, version and login, read from the
/// cleaned environment runs use.
#[tauri::command]
pub async fn get_claude_plan_status(state: State<'_, Arc<AppState>>) -> Result<ClaudePlanStatus> {
    let claude = state.claude.clone();
    // Two short `claude` subprocesses
    tauri::async_runtime::spawn_blocking(move || claude.status())
        .await
        .map_err(|e| e.to_string())
}

/// Sends one small request in the production shape to `model` or the saved model, through
/// `access` or the saved access: with the configured key, or through `claude` on your Claude
/// plan. Errors come back in the result, for Settings to show.
#[tauri::command]
pub async fn test_provider(
    provider: Provider,
    model: Option<String>,
    access: Option<AnthropicAccess>,
    state: State<'_, Arc<AppState>>,
) -> Result<ConnectionTest> {
    match provider {
        Provider::Anthropic => {
            let saved = settings::load(&state.db.master_pool)
                .await
                .map_err(|e| e.to_string())?
                .anthropic;
            let model = match model.map(|m| m.trim().to_string()) {
                Some(model) if !model.is_empty() => model,
                _ => saved.model,
            };
            Ok(match access.unwrap_or(saved.access) {
                AnthropicAccess::ApiKey => state.anthropic.test_connection(&model).await,
                AnthropicAccess::ClaudePlan => state.claude.test_connection(&model).await,
            })
        }
        Provider::Jev => Err("Jev's connection test isn't built yet".into()),
    }
}

#[cfg(test)]
mod tests {
    use crate::autotag::anthropic::{mock, AnthropicProvider};
    use crate::autotag::claude_cli::ClaudeCli;
    use crate::autotag::credentials::{Credentials, MemoryKeyring, Provider};
    use crate::autotag::engine::{Engine, RunScope};
    use crate::autotag::provider::Mode;
    use crate::autotag::settings;
    use crate::db::{insert_asset, insert_tag, memory_library, memory_library_pool, Database};
    use serde_json::json;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const SAVED_SENTINEL: &str = "sk-ant-SENTINEL-saved-7d1f0c";
    const ENV_SENTINEL: &str = "sk-ant-SENTINEL-env-93be4a";
    const JEV_SENTINEL: &str = "ts-SENTINEL-jev-51aa02";

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Saves, reads, overrides and clears sentinel keys, saves settings, and runs an Anthropic
    /// tagging run and "Test connection" against a local stand-in for the API, with every
    /// `tracing` event captured. No sentinel may reach the master DB file, the log, any returned
    /// payload or any `autotag:updated` event.
    #[tokio::test]
    async fn api_keys_never_reach_the_master_db_the_log_or_a_payload() {
        let log = Captured::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(dir.path().join("opendam.sqlite").to_str().unwrap())
            .await
            .unwrap();
        let creds = Arc::new(Credentials::new(MemoryKeyring::default(), |name| {
            (name == "ANTHROPIC_API_KEY").then(|| ENV_SENTINEL.to_string())
        }));

        let mut payloads = Vec::new();
        payloads.push(json!(creds
            .set_key(Provider::Anthropic, Some(SAVED_SENTINEL))
            .unwrap()));
        payloads.push(json!(creds
            .set_key(Provider::Jev, Some(JEV_SENTINEL))
            .unwrap()));
        payloads.push(json!(creds.statuses()));
        payloads.push(json!(settings::update(
            &db.master_pool,
            json!({"anthropic": {"model": "claude-haiku-5-5"}, "jev": {"maxTags": 4}}),
        )
        .await
        .unwrap()));
        payloads.push(json!(settings::load(&db.master_pool).await.unwrap()));
        assert_eq!(
            creds.api_key(Provider::Anthropic).unwrap().as_deref(),
            Some(ENV_SENTINEL)
        );
        assert_eq!(
            creds.api_key(Provider::Jev).unwrap().as_deref(),
            Some(JEV_SENTINEL)
        );

        // A run and a connection test through the Anthropic provider
        let lib_pool = memory_library_pool().await;
        insert_asset(&lib_pool, "a1").await;
        db.libraries
            .write()
            .await
            .insert("lib".into(), memory_library("lib", lib_pool));
        for id in ["nature", "rock", "tree"] {
            insert_tag(&db.master_pool, id, id).await;
        }
        let api = mock::serve(
            200,
            &[],
            include_str!("../autotag/fixtures/anthropic/unknown_ids.json"),
        )
        .await;
        let anthropic = Arc::new(AnthropicProvider::with_base_url(
            creds.clone(),
            Arc::new(ClaudeCli::unused()),
            api.url.clone(),
        ));
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = events.clone();
        let db = Arc::new(db);
        let engine = Engine::new(
            db.clone(),
            vec![anthropic.clone()],
            Arc::new(move |update| recorded.lock().unwrap().push(json!(update))),
            Arc::new(|_| {}),
        );
        engine.start().await;
        payloads.push(json!(engine
            .start_run(
                RunScope::Assets {
                    asset_ids: vec!["a1".into()],
                },
                "anthropic",
                Mode::ExistingOnly,
            )
            .await
            .unwrap()));
        for _ in 0..500 {
            if engine.list_runs().await.unwrap()[0].status == "done" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        let runs = engine.list_runs().await.unwrap();
        assert_eq!((runs[0].status.as_str(), runs[0].done), ("done", 1));
        assert!(runs[0].cost_usd > 0.0, "the run's running cost");
        payloads.push(json!(runs));
        payloads.push(json!(engine.provider_options().await.unwrap()));
        payloads.push(json!(anthropic.test_connection("claude-sonnet-5-5").await));
        let received = api.received();
        assert_eq!(received.len(), 2, "the run's item and the connection test");
        for request in &received {
            assert!(
                request
                    .head
                    .contains(&format!("x-api-key: {}", ENV_SENTINEL.to_lowercase())),
                "the key goes to the API, and only in its header"
            );
            assert!(!request.body.contains(ENV_SENTINEL));
        }
        let events = events.lock().unwrap().clone();
        assert!(!events.is_empty(), "the run emitted events");

        payloads.push(json!(creds.set_key(Provider::Anthropic, None).unwrap()));
        db.master_pool.close().await;

        let sentinels = [SAVED_SENTINEL, ENV_SENTINEL, JEV_SENTINEL];
        let mut db_bytes = Vec::new();
        for file in std::fs::read_dir(dir.path()).unwrap() {
            db_bytes.extend(std::fs::read(file.unwrap().path()).unwrap());
        }
        let db_text = String::from_utf8_lossy(&db_bytes);
        assert!(
            db_text.contains("ai.anthropic.model"),
            "settings were saved"
        );
        let log_text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(log_text.contains("Saved the Anthropic API key in the keyring"));
        let payload_text = serde_json::to_string(&payloads).unwrap();
        assert!(payload_text.contains("from ANTHROPIC_API_KEY"));
        let event_text = serde_json::to_string(&events).unwrap();
        assert!(event_text.contains("changedAssetIds"));

        for sentinel in sentinels {
            assert!(!db_text.contains(sentinel), "{sentinel} in the master DB");
            assert!(!log_text.contains(sentinel), "{sentinel} in the log");
            assert!(!payload_text.contains(sentinel), "{sentinel} in a payload");
            assert!(!event_text.contains(sentinel), "{sentinel} in an event");
        }
    }
}
