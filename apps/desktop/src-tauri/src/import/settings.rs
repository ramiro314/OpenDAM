//! Pack import settings, one row per field in the master DB's `settings` table
//! (`import.<field>`, JSON value), as the export's are: the memory caps Blender (`unity_glb`)
//! and the Unreal Editor (`export_unreal`) run under, where the Unreal Editor is, and the
//! `assimp` program `unity_glb` converts ASCII FBX files with (plan pack-import, Runs, Capped
//! processes, and Settings). An unset field has no row.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Pool, Sqlite};

use crate::capped::check_cap;

const BLENDER_MEMORY_MAX: &str = "blenderMemoryMax";
const UNREAL_MEMORY_MAX: &str = "unrealMemoryMax";
const UNREAL_EDITOR_ROOT: &str = "unrealEditorRoot";
const ASSIMP_PATH: &str = "assimpPath";
const FIELDS: [&str; 4] = [
    BLENDER_MEMORY_MAX,
    UNREAL_MEMORY_MAX,
    UNREAL_EDITOR_ROOT,
    ASSIMP_PATH,
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSettings {
    /// The `MemoryMax` Blender runs under, `12G` when unset.
    pub blender_memory_max: Option<String>,
    /// The `MemoryMax` the Unreal Editor runs under, `20G` when unset.
    pub unreal_memory_max: Option<String>,
    /// The folder the Unreal Editor's Linux zip was extracted to, holding
    /// `Engine/Binaries/Linux/UnrealEditor`: `~/Applications/UnrealEngine` when unset.
    pub unreal_editor_root: Option<String>,
    /// The `assimp` program, `assimp` on `PATH` when unset.
    pub assimp_path: Option<String>,
}

impl ImportSettings {
    pub fn blender_memory_max(&self) -> String {
        self.blender_memory_max
            .clone()
            .unwrap_or_else(|| super::unity_glb::DEFAULT_MEMORY_MAX.to_string())
    }

    pub fn unreal_memory_max(&self) -> String {
        self.unreal_memory_max
            .clone()
            .unwrap_or_else(|| super::unreal_glb::DEFAULT_MEMORY_MAX.to_string())
    }

    pub fn unreal_editor_root(&self) -> PathBuf {
        self.unreal_editor_root
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(super::unreal_glb::default_editor_root)
    }

    /// The `assimp` program a run starts: the setting's path, else `assimp` on `PATH`.
    pub fn assimp(&self) -> String {
        self.assimp_path
            .clone()
            .unwrap_or_else(|| super::tools::ASSIMP.to_string())
    }

    fn field(&mut self, field: &str) -> &mut Option<String> {
        match field {
            BLENDER_MEMORY_MAX => &mut self.blender_memory_max,
            UNREAL_MEMORY_MAX => &mut self.unreal_memory_max,
            ASSIMP_PATH => &mut self.assimp_path,
            _ => &mut self.unreal_editor_root,
        }
    }
}

fn key(field: &str) -> String {
    format!("import.{field}")
}

/// Whether `value` is one field `field` can hold: a cap systemd reads, an absolute folder, or
/// an absolute program.
pub fn check(field: &str, value: &str) -> Result<(), String> {
    match field {
        UNREAL_EDITOR_ROOT if !value.starts_with('/') => Err(format!(
            "import.{field} must be an absolute folder, not {value:?}"
        )),
        ASSIMP_PATH if !value.starts_with('/') => Err(format!(
            "import.{field} must be an absolute path, not {value:?}"
        )),
        UNREAL_EDITOR_ROOT | ASSIMP_PATH => Ok(()),
        _ => check_cap(value),
    }
}

/// The saved settings. A saved value a field can't hold counts as unset.
pub async fn load(pool: &Pool<Sqlite>) -> sqlx::Result<ImportSettings> {
    let mut settings = ImportSettings::default();
    for field in FIELDS {
        let raw: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
            .bind(key(field))
            .fetch_optional(pool)
            .await?;
        let Some(raw) = raw else { continue };
        match serde_json::from_str::<String>(&raw) {
            Ok(value) if check(field, &value).is_ok() => *settings.field(field) = Some(value),
            _ => tracing::warn!("Ignoring the saved setting {}: {raw}", key(field)),
        }
    }
    Ok(settings)
}

/// Applies `patch`, such as `{"blenderMemoryMax": "16G"}`, where null clears a field. An unknown
/// field, a wrong type, a cap systemd can't read or a relative folder saves nothing.
pub async fn update(pool: &Pool<Sqlite>, patch: Value) -> Result<ImportSettings, String> {
    let Value::Object(fields) = patch else {
        return Err("Import settings patch must be an object".into());
    };
    let mut changed = Vec::new();
    for (field, value) in fields {
        if !FIELDS.contains(&field.as_str()) {
            return Err(format!("Unknown setting import.{field}"));
        }
        let value = match value {
            Value::Null => None,
            Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
            _ => return Err(format!("import.{field} must be a string or null")),
        };
        if let Some(value) = &value {
            check(&field, value)?;
        }
        changed.push((key(&field), value));
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
            json!({"unrealMemoryMax": "0G"}),
            json!({"unrealEditorRoot": "Applications/UnrealEngine"}),
            json!({"unrealEditor": "/opt/UnrealEngine"}),
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

    #[tokio::test]
    async fn the_unreal_editor_defaults_to_applications_with_a_20g_cap() {
        let db = crate::db::memory_database(vec![]).await;
        let pool = &db.master_pool;
        let unset = load(pool).await.unwrap();
        assert_eq!(unset.unreal_memory_max(), "20G");
        assert_eq!(
            unset.unreal_editor_root(),
            crate::import::unreal_glb::default_editor_root()
        );
        assert!(unset
            .unreal_editor_root()
            .ends_with("Applications/UnrealEngine"));

        let saved = update(
            pool,
            json!({"unrealMemoryMax": "24G", "unrealEditorRoot": "/opt/UnrealEngine"}),
        )
        .await
        .unwrap();
        assert_eq!(saved.unreal_memory_max(), "24G");
        assert_eq!(
            saved.unreal_editor_root(),
            PathBuf::from("/opt/UnrealEngine")
        );
        // Each field is its own row: Blender's cap is still unset.
        assert_eq!(saved.blender_memory_max(), "12G");
        assert_eq!(load(pool).await.unwrap(), saved);

        let cleared = update(pool, json!({"unrealEditorRoot": null}))
            .await
            .unwrap();
        assert_eq!(cleared.unreal_memory_max(), "24G");
        assert_eq!(cleared.unreal_editor_root, None);
    }

    #[tokio::test]
    async fn assimp_is_on_path_unless_an_absolute_path_is_saved() {
        let db = crate::db::memory_database(vec![]).await;
        let pool = &db.master_pool;
        assert_eq!(load(pool).await.unwrap().assimp(), "assimp");

        let saved = update(pool, json!({"assimpPath": " /opt/assimp/bin/assimp "}))
            .await
            .unwrap();
        assert_eq!(saved.assimp(), "/opt/assimp/bin/assimp");
        assert_eq!(load(pool).await.unwrap(), saved);

        for bad in [json!({"assimpPath": "assimp"}), json!({"assimpPath": 1})] {
            assert!(update(pool, bad.clone()).await.is_err(), "{bad}");
        }
        assert_eq!(load(pool).await.unwrap().assimp(), "/opt/assimp/bin/assimp");

        // The convert step runs the saved program
        let (worker, _requests) = tokio::sync::mpsc::channel(1);
        let tools = crate::import::unity_glb::Tools::load(pool, PathBuf::new(), worker).await;
        assert_eq!(tools.assimp, "/opt/assimp/bin/assimp");

        let cleared = update(pool, json!({"assimpPath": ""})).await.unwrap();
        assert_eq!(cleared.assimp(), "assimp");
    }
}
