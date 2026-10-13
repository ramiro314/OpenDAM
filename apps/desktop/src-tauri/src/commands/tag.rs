use std::sync::Arc;
use tauri::State;
use uuid::Uuid;

use crate::db::{upsert_tag, Database};
use crate::models::Tag;
use crate::AppState;

type Result<T> = std::result::Result<T, String>;

#[tauri::command]
pub async fn list_tags(state: State<'_, Arc<AppState>>) -> Result<Vec<Tag>> {
    list_tags_in(&state.db).await
}

/// Tags come from the master catalog, never from a library's mirror.
async fn list_tags_in(db: &Database) -> Result<Vec<Tag>> {
    let rows = sqlx::query_as::<_, (String, String, Option<String>, Option<String>)>(
        "SELECT id, name, color, parent_id FROM tags ORDER BY name ASC",
    )
    .fetch_all(&db.master_pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(rows
        .into_iter()
        .map(|(id, name, color, parent_id)| Tag {
            id,
            name,
            color,
            parent_id,
        })
        .collect())
}

#[tauri::command]
pub async fn create_tag(
    name: String,
    parent_id: Option<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<Tag> {
    let id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO tags (id, name, parent_id) VALUES (?, ?, ?)")
        .bind(&id)
        .bind(&name)
        .bind(&parent_id)
        .execute(&state.db.master_pool)
        .await
        .map_err(|e| e.to_string())?;

    let row = (id, name, None, parent_id);
    for lib in state.db.get_all_libraries().await {
        if let Err(e) = upsert_tag(&lib.pool, &row).await {
            tracing::warn!(
                "Failed to mirror tag {} to library {}: {}",
                row.0,
                lib.id,
                e
            );
        }
    }

    let (id, name, color, parent_id) = row;
    Ok(Tag {
        id,
        name,
        color,
        parent_id,
    })
}

#[tauri::command]
pub async fn delete_tag(id: String, state: State<'_, Arc<AppState>>) -> Result<()> {
    sqlx::query("DELETE FROM tags WHERE id = ?")
        .bind(&id)
        .execute(&state.db.master_pool)
        .await
        .map_err(|e| e.to_string())?;

    // Memberships are deleted explicitly: not every library connection enforces the cascade.
    for lib in state.db.get_all_libraries().await {
        let _ = sqlx::query("DELETE FROM asset_tags WHERE tag_id = ?")
            .bind(&id)
            .execute(&lib.pool)
            .await;
        let _ = sqlx::query("DELETE FROM tags WHERE id = ?")
            .bind(&id)
            .execute(&lib.pool)
            .await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{insert_tag, memory_database, memory_library, memory_library_pool};

    #[tokio::test]
    async fn list_tags_returns_the_master_catalog_names() {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        insert_tag(&a, "t", "mirror name").await;
        insert_tag(&b, "t", "mirror name").await;
        let db = memory_database(vec![memory_library("a", a), memory_library("b", b)]).await;
        insert_tag(&db.master_pool, "t", "master name").await;

        let names: Vec<String> = list_tags_in(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["master name"]);
    }
}
