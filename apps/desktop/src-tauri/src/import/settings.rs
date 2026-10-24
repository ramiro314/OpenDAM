//! Pack import settings, one row per field in the master DB's `settings` table
//! (`import.<field>`, JSON value), as the export's are: Blender's memory cap for the `unity_glb`
//! step (plan pack-import, Runs, Capped processes). An unset field has no row.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Pool, Sqlite};

use super::unity_glb::DEFAULT_MEMORY_MAX;
use crate::capped::check_cap;

const BLENDER_MEMORY_MAX_KEY: &str = "import.blenderMemoryMax";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSettings {
    /// The `MemoryMax` Blender runs under, `12G` when unset.
    pub blender_memory_max: Option<String>,
}

impl ImportSettings {
    pub fn blender_memory_max(&self) -> String {
        self.blender_memory_max
            .clone()
            .unwrap_or_else(|| DEFAULT_MEMORY_MAX.to_string())
    }
}

/// The saved settings. A saved value that isn't a valid cap counts as unset.
pub async fn load(pool: &Pool<Sqlite>) -> sqlx::Result<ImportSettings> {
    let raw: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
        .bind(BLENDER_MEMORY_MAX_KEY)
        .fetch_optional(pool)
        .await?;
    let mut settings = ImportSettings::default();
    if let Some(raw) = raw {
        match serde_json::from_str::<String>(&raw) {
            Ok(cap) if check_cap(&cap).is_ok() => settings.blender_memory_max = Some(cap),
            _ => tracing::warn!("Ignoring the saved setting {BLENDER_MEMORY_MAX_KEY}: {raw}"),
        }
    }
    Ok(settings)
}

/// Applies `patch`, such as `{"blenderMemoryMax": "16G"}`, where null clears a field. An unknown
/// field, a wrong type or a cap systemd can't read saves nothing.
pub async fn update(pool: &Pool<Sqlite>, patch: Value) -> Result<ImportSettings, String> {
    let Value::Object(fields) = patch else {
        return Err("Import settings patch must be an object".into());
    };
    let mut changed = Vec::new();
    for (field, value) in fields {
        let key = match field.as_str() {
            "blenderMemoryMax" => BLENDER_MEMORY_MAX_KEY,
            _ => return Err(format!("Unknown setting import.{field}")),
        };
        let value = match value {
            Value::Null => None,
            Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
            _ => return Err(format!("import.{field} must be a string or null")),
        };
        if let Some(value) = &value {
            check_cap(value)?;
        }
        changed.push((key, value));
    }

    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    for (key, value) in &changed {
        match value {
            Some(value) => sqlx::query(
                "INSERT INTO settings (key, value) VALUES (?, ?)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(key)
            .bind(Value::String(value.clone()).to_string()),
            None => sqlx::query("DELETE FROM settings WHERE key = ?").bind(key),
        }
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    }
    tx.commit().await.map_err(|e| e.to_string())?;
    load(pool).await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn the_cap_defaults_to_12g_and_saves_only_what_systemd_reads() {
        let db = crate::db::memory_database(vec![]).await;
        let pool = &db.master_pool;
        let unset = load(pool).await.unwrap();
        assert_eq!(unset, ImportSettings::default());
        assert_eq!(unset.blender_memory_max(), "12G");

        let saved = update(pool, json!({"blenderMemoryMax": " 16G "}))
            .await
            .unwrap();
        assert_eq!(saved.blender_memory_max(), "16G");
        assert_eq!(load(pool).await.unwrap(), saved);

        for bad in [
            json!({"blenderMemoryMax": "lots"}),
            json!({"blenderMemoryMax": 16}),
            json!({"unrealMemoryMax": "20G"}),
            json!("16G"),
        ] {
            assert!(update(pool, bad.clone()).await.is_err(), "{bad}");
        }
        assert_eq!(load(pool).await.unwrap().blender_memory_max(), "16G");

        let cleared = update(pool, json!({"blenderMemoryMax": null}))
            .await
            .unwrap();
        assert_eq!(cleared.blender_memory_max(), "12G");
    }
}
