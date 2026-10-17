use std::sync::Arc;
use tauri::State;

use crate::autotag::credentials::{Provider, ProviderStatus};
use crate::autotag::settings::{self, AiSettings};
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

#[cfg(test)]
mod tests {
    use crate::autotag::credentials::{Credentials, MemoryKeyring, Provider};
    use crate::autotag::settings;
    use crate::db::Database;
    use serde_json::json;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

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

    /// Saves, reads, overrides and clears sentinel keys, and saves settings, with every
    /// `tracing` event captured. No sentinel may reach the master DB file, the log or any
    /// returned payload.
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
        let creds = Credentials::new(MemoryKeyring::default(), |name| {
            (name == "ANTHROPIC_API_KEY").then(|| ENV_SENTINEL.to_string())
        });

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

        for sentinel in sentinels {
            assert!(!db_text.contains(sentinel), "{sentinel} in the master DB");
            assert!(!log_text.contains(sentinel), "{sentinel} in the log");
            assert!(!payload_text.contains(sentinel), "{sentinel} in a payload");
        }
    }
}
