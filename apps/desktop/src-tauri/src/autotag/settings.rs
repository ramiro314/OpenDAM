//! Non-secret AI tagging preferences, one row per field in the master DB's `settings` table
//! (`ai.<provider>.<field>`, JSON value), and the provider and mode of the last run started
//! (`autotag.lastUsed`). API keys never go here: see `credentials`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Pool, Sqlite};

use super::provider::Mode;

const PREFIX: &str = "ai.";
const LAST_USED_KEY: &str = "autotag.lastUsed";

pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-5-5";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiSettings {
    pub anthropic: AnthropicSettings,
    pub jev: JevSettings,
}

/// How Anthropic requests reach Claude (D11).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicAccess {
    /// The Messages API over HTTPS, with the key from the keyring or `ANTHROPIC_API_KEY`.
    #[default]
    ApiKey,
    /// Your Claude plan, through the installed `claude` CLI (`claude_cli`). Personal use only.
    ClaudePlan,
}

impl AnthropicAccess {
    /// Stored in `tag_runs.access`.
    pub fn as_str(self) -> &'static str {
        match self {
            AnthropicAccess::ApiKey => "api_key",
            AnthropicAccess::ClaudePlan => "claude_plan",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnthropicSettings {
    pub access: AnthropicAccess,
    /// A model id: one of the Claude 5.5 models or any custom id. It applies to both accesses.
    pub model: String,
    /// Requests in flight at once, shared by every run.
    pub concurrency: u32,
    pub max_tags: u32,
    pub max_proposals: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JevSettings {
    pub concurrency: u32,
    /// Tags scoring at least this (0-1) are kept.
    pub threshold: f64,
    pub max_tags: u32,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            anthropic: AnthropicSettings {
                access: AnthropicAccess::ApiKey,
                model: DEFAULT_ANTHROPIC_MODEL.to_string(),
                concurrency: 4,
                max_tags: 8,
                max_proposals: 3,
            },
            jev: JevSettings {
                concurrency: 4,
                threshold: 0.7,
                max_tags: 8,
            },
        }
    }
}

impl AiSettings {
    fn validate(&self) -> Result<(), String> {
        let model = &self.anthropic.model;
        if model.is_empty() || model.len() > 128 || model.chars().any(char::is_whitespace) {
            return Err("The model id must be 1-128 characters with no spaces".into());
        }
        in_range("Anthropic concurrency", self.anthropic.concurrency, 1, 16)?;
        in_range("Anthropic max tags", self.anthropic.max_tags, 1, 50)?;
        in_range(
            "Anthropic max proposals",
            self.anthropic.max_proposals,
            0,
            20,
        )?;
        in_range("Jev concurrency", self.jev.concurrency, 1, 16)?;
        in_range("Jev max tags", self.jev.max_tags, 1, 50)?;
        if !(0.0..=1.0).contains(&self.jev.threshold) {
            return Err("Jev threshold must be between 0 and 1".into());
        }
        Ok(())
    }
}

fn in_range(what: &str, value: u32, min: u32, max: u32) -> Result<(), String> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(format!("{what} must be between {min} and {max}"))
    }
}

fn parse(value: Value) -> Result<AiSettings, String> {
    let settings: AiSettings = serde_json::from_value(value).map_err(|e| e.to_string())?;
    settings.validate()?;
    Ok(settings)
}

/// The slot for `ai.<provider>.<field>` in a serialized `AiSettings`, if that field exists.
fn slot<'a>(value: &'a mut Value, provider: &str, field: &str) -> Option<&'a mut Value> {
    value.get_mut(provider)?.get_mut(field)
}

/// The saved settings, with defaults for fields never saved. A saved value that no longer
/// parses or is out of range falls back to its default.
pub async fn load(pool: &Pool<Sqlite>) -> sqlx::Result<AiSettings> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM settings WHERE key LIKE 'ai.%'")
            .fetch_all(pool)
            .await?;

    let mut value = serde_json::to_value(AiSettings::default()).expect("settings serialize");
    for (key, raw) in rows {
        let Some((provider, field)) = key.strip_prefix(PREFIX).and_then(|k| k.split_once('.'))
        else {
            continue;
        };
        let Ok(saved) = serde_json::from_str::<Value>(&raw) else {
            tracing::warn!("Ignoring the saved setting {key}: not JSON");
            continue;
        };
        let mut candidate = value.clone();
        let Some(slot) = slot(&mut candidate, provider, field) else {
            continue;
        };
        *slot = saved;
        match parse(candidate.clone()) {
            Ok(_) => value = candidate,
            Err(e) => tracing::warn!("Ignoring the saved setting {key}: {e}"),
        }
    }
    Ok(parse(value).unwrap_or_default())
}

/// Applies `patch`, a partial `AiSettings` such as `{"anthropic": {"model": "..."}}`, and saves
/// the fields it names in one transaction. An unknown field, a wrong type or an out-of-range
/// value saves nothing.
pub async fn update(pool: &Pool<Sqlite>, patch: Value) -> Result<AiSettings, String> {
    let current = load(pool).await.map_err(|e| e.to_string())?;
    let mut value = serde_json::to_value(current).expect("settings serialize");

    let Value::Object(providers) = patch else {
        return Err("Settings patch must be an object".into());
    };
    let mut changed = Vec::new();
    for (provider, fields) in providers {
        let Value::Object(fields) = fields else {
            return Err(format!("Settings for {provider} must be an object"));
        };
        for (field, new) in fields {
            let slot = slot(&mut value, &provider, &field)
                .ok_or_else(|| format!("Unknown setting {provider}.{field}"))?;
            *slot = new.clone();
            changed.push((format!("{PREFIX}{provider}.{field}"), new.to_string()));
        }
    }
    let settings = parse(value)?;

    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    for (key, json) in &changed {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(json)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(settings)
}

/// The provider and mode the last run started with, which "Suggest tags" uses (D32).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastUsed {
    pub provider: String,
    pub mode: Mode,
}

impl Default for LastUsed {
    /// Before any run: Anthropic, Prefer existing.
    fn default() -> Self {
        Self {
            provider: "anthropic".into(),
            mode: Mode::PreferExisting,
        }
    }
}

/// The last-used provider and mode, or the defaults when no run has started or the saved value
/// no longer parses.
pub async fn last_used(pool: &Pool<Sqlite>) -> sqlx::Result<LastUsed> {
    let raw: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
        .bind(LAST_USED_KEY)
        .fetch_optional(pool)
        .await?;
    Ok(raw
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default())
}

pub async fn save_last_used(pool: &Pool<Sqlite>, last: &LastUsed) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(LAST_USED_KEY)
    .bind(serde_json::to_string(last).expect("last used serializes"))
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memory_database;
    use serde_json::json;

    async fn saved_rows(pool: &Pool<Sqlite>) -> Vec<(String, String)> {
        sqlx::query_as("SELECT key, value FROM settings ORDER BY key")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_empty_table_gives_the_plan_defaults() {
        let db = memory_database(vec![]).await;

        let settings = load(&db.master_pool).await.unwrap();

        assert_eq!(settings.anthropic.access, AnthropicAccess::ApiKey);
        assert_eq!(settings.anthropic.model, "claude-sonnet-5-5");
        assert_eq!(settings.anthropic.concurrency, 4);
        assert_eq!(settings.anthropic.max_tags, 8);
        assert_eq!(settings.anthropic.max_proposals, 3);
        assert_eq!(settings.jev.concurrency, 4);
        assert_eq!(settings.jev.threshold, 0.7);
        assert_eq!(settings.jev.max_tags, 8);
    }

    #[tokio::test]
    async fn settings_round_trip_through_the_master_db() {
        let db = memory_database(vec![]).await;

        let saved = update(
            &db.master_pool,
            json!({
                "anthropic": {"access": "claude_plan", "model": "my-custom-model", "concurrency": 2, "maxTags": 5, "maxProposals": 0},
                "jev": {"concurrency": 8, "threshold": 0.85, "maxTags": 3},
            }),
        )
        .await
        .unwrap();

        let loaded = load(&db.master_pool).await.unwrap();
        assert_eq!(loaded, saved);
        assert_eq!(loaded.anthropic.access, AnthropicAccess::ClaudePlan);
        assert_eq!(loaded.anthropic.model, "my-custom-model");
        assert_eq!(loaded.anthropic.max_proposals, 0);
        assert_eq!(loaded.jev.threshold, 0.85);
    }

    #[tokio::test]
    async fn a_patch_saves_only_the_fields_it_names() {
        let db = memory_database(vec![]).await;

        let settings = update(&db.master_pool, json!({"jev": {"maxTags": 12}}))
            .await
            .unwrap();

        assert_eq!(settings.jev.max_tags, 12);
        assert_eq!(settings.anthropic, AiSettings::default().anthropic);
        assert_eq!(
            saved_rows(&db.master_pool).await,
            [("ai.jev.maxTags".to_string(), "12".to_string())]
        );
    }

    #[tokio::test]
    async fn an_invalid_patch_is_an_error_and_saves_nothing() {
        let db = memory_database(vec![]).await;

        for patch in [
            json!({"anthropic": {"concurrency": 0, "maxTags": 4}}),
            json!({"anthropic": {"maxTags": 4, "colour": "red"}}),
            json!({"jev": {"threshold": 1.5}}),
            json!({"jev": {"maxTags": "eight"}}),
            json!({"anthropic": {"model": "has spaces"}}),
            json!({"anthropic": {"access": "oauth"}}),
            json!({"openai": {"model": "x"}}),
            json!(["not", "an", "object"]),
        ] {
            assert!(
                update(&db.master_pool, patch.clone()).await.is_err(),
                "{patch}"
            );
        }
        assert!(saved_rows(&db.master_pool).await.is_empty());
    }

    #[tokio::test]
    async fn every_range_includes_its_bounds_and_nothing_past_them() {
        let db = memory_database(vec![]).await;

        for patch in [
            json!({"anthropic": {"concurrency": 17}}),
            json!({"anthropic": {"maxTags": 51}}),
            json!({"anthropic": {"maxProposals": 21}}),
            json!({"anthropic": {"maxTags": 0}}),
            json!({"jev": {"concurrency": 17}}),
            json!({"jev": {"maxTags": 51}}),
            json!({"jev": {"threshold": -0.01}}),
            json!({"jev": {"threshold": 1.01}}),
        ] {
            assert!(
                update(&db.master_pool, patch.clone()).await.is_err(),
                "{patch}"
            );
        }
        assert!(saved_rows(&db.master_pool).await.is_empty());

        let at_max = update(
            &db.master_pool,
            json!({
                "anthropic": {"concurrency": 16, "maxTags": 50, "maxProposals": 20},
                "jev": {"concurrency": 16, "threshold": 1.0, "maxTags": 50},
            }),
        )
        .await
        .unwrap();
        assert_eq!(at_max.anthropic.concurrency, 16);
        assert_eq!(at_max.anthropic.max_tags, 50);
        assert_eq!(at_max.anthropic.max_proposals, 20);
        assert_eq!(at_max.jev.concurrency, 16);
        assert_eq!(at_max.jev.threshold, 1.0);
        assert_eq!(at_max.jev.max_tags, 50);

        let at_min = update(
            &db.master_pool,
            json!({
                "anthropic": {"concurrency": 1, "maxTags": 1, "maxProposals": 0},
                "jev": {"concurrency": 1, "threshold": 0.0, "maxTags": 1},
            }),
        )
        .await
        .unwrap();
        assert_eq!(load(&db.master_pool).await.unwrap(), at_min);
        assert_eq!(at_min.jev.threshold, 0.0);
        assert_eq!(at_min.anthropic.max_proposals, 0);
    }

    #[tokio::test]
    async fn a_saved_value_that_is_no_longer_valid_falls_back_to_its_default() {
        let db = memory_database(vec![]).await;
        for (key, value) in [
            ("ai.anthropic.concurrency", "99"),
            ("ai.anthropic.maxTags", "\"many\""),
            ("ai.jev.maxTags", "5"),
            ("ai.jev.retired", "1"),
        ] {
            sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?)")
                .bind(key)
                .bind(value)
                .execute(&db.master_pool)
                .await
                .unwrap();
        }

        let settings = load(&db.master_pool).await.unwrap();

        assert_eq!(settings.anthropic.concurrency, 4);
        assert_eq!(settings.anthropic.max_tags, 8);
        assert_eq!(settings.jev.max_tags, 5);
    }

    #[tokio::test]
    async fn last_used_defaults_to_anthropic_prefer_existing_and_round_trips() {
        let db = memory_database(vec![]).await;
        let pool = &db.master_pool;

        assert_eq!(
            last_used(pool).await.unwrap(),
            LastUsed {
                provider: "anthropic".into(),
                mode: Mode::PreferExisting,
            }
        );

        let jev = LastUsed {
            provider: "jev".into(),
            mode: Mode::ExistingOnly,
        };
        save_last_used(pool, &jev).await.unwrap();
        assert_eq!(last_used(pool).await.unwrap(), jev);
        assert_eq!(load(pool).await.unwrap(), AiSettings::default());

        sqlx::query("UPDATE settings SET value = '\"garbled\"' WHERE key = ?")
            .bind(LAST_USED_KEY)
            .execute(pool)
            .await
            .unwrap();
        assert_eq!(last_used(pool).await.unwrap(), LastUsed::default());
    }
}
