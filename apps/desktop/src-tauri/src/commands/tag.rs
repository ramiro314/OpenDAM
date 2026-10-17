use std::sync::Arc;
use tauri::State;
use uuid::Uuid;

use crate::db::{create_master_tag, upsert_tag, Database, TagRow};
use crate::models::{Tag, TagPatch};
use crate::AppState;

type Result<T> = std::result::Result<T, String>;

#[tauri::command]
pub async fn list_tags(state: State<'_, Arc<AppState>>) -> Result<Vec<Tag>> {
    list_tags_in(&state.db).await
}

/// Tags come from the master catalog, never from a library's mirror.
async fn list_tags_in(db: &Database) -> Result<Vec<Tag>> {
    let rows = sqlx::query_as::<_, TagRow>(
        "SELECT id, name, color, parent_id, description FROM tags ORDER BY name ASC",
    )
    .fetch_all(&db.master_pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(rows.into_iter().map(tag_from_row).collect())
}

fn tag_from_row((id, name, color, parent_id, description): TagRow) -> Tag {
    Tag {
        id,
        name,
        color,
        parent_id,
        description,
    }
}

#[tauri::command]
pub async fn create_tag(
    name: String,
    parent_id: Option<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<Tag> {
    create_tag_in(&state.db, name, parent_id).await
}

/// Writes the definition to master, then mirrors it to every open library.
async fn create_tag_in(db: &Database, name: String, parent_id: Option<String>) -> Result<Tag> {
    let id = Uuid::new_v4().to_string();
    let mut conn = db.master_pool.acquire().await.map_err(|e| e.to_string())?;
    create_master_tag(&mut conn, &id, &name, parent_id.as_deref())
        .await
        .map_err(|e| e.to_string())?;
    drop(conn);

    let row = (id, name, None, parent_id, String::new());
    for lib in db.get_all_libraries().await {
        if let Err(e) = upsert_tag(&lib.pool, &row).await {
            tracing::warn!(
                "Failed to mirror tag {} to library {}: {}",
                row.0,
                lib.id,
                e
            );
        }
    }

    Ok(tag_from_row(row))
}

#[tauri::command]
pub async fn update_tag(
    id: String,
    patch: TagPatch,
    state: State<'_, Arc<AppState>>,
) -> Result<Tag> {
    update_tag_in(&state.db, &id, patch).await
}

/// Writes the changed definition to master, then mirrors it, with its ancestors, to every open
/// library in place, so its memberships stay.
async fn update_tag_in(db: &Database, id: &str, patch: TagPatch) -> Result<Tag> {
    let (_, mut name, mut color, mut parent_id, mut description) = sqlx::query_as::<_, TagRow>(
        "SELECT id, name, color, parent_id, description FROM tags WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&db.master_pool)
    .await
    .map_err(|e| e.to_string())?
    .ok_or("The tag no longer exists")?;

    if let Some(new_name) = patch.name {
        name = new_name.trim().to_string();
        if name.is_empty() {
            return Err("A tag needs a name".into());
        }
    }
    if let Some(new_color) = patch.color {
        color = new_color.filter(|c| !c.is_empty() && c != "none");
    }
    if let Some(new_parent) = patch.parent_id {
        if let Some(parent) = &new_parent {
            let ancestors = db
                .tag_with_ancestors(parent)
                .await
                .map_err(|e| e.to_string())?;
            if ancestors.is_empty() {
                return Err("The parent tag no longer exists".into());
            }
            if ancestors.iter().any(|t| t.0 == id) {
                return Err(format!(
                    "\"{name}\" can't go under itself or a tag nested in it"
                ));
            }
        }
        parent_id = new_parent;
    }
    if let Some(new_description) = patch.description {
        description = new_description.trim().to_string();
    }

    // Case-insensitive, like the sidebar's check when a tag is created. Folded in Rust, since
    // SQLite's lower() only folds ASCII.
    let others: Vec<String> = sqlx::query_scalar("SELECT name FROM tags WHERE id != ?")
        .bind(id)
        .fetch_all(&db.master_pool)
        .await
        .map_err(|e| e.to_string())?;
    let folded = name.to_lowercase();
    if let Some(taken) = others.into_iter().find(|n| n.to_lowercase() == folded) {
        return Err(format!("A tag named \"{taken}\" already exists"));
    }

    sqlx::query("UPDATE tags SET name = ?, color = ?, parent_id = ?, description = ? WHERE id = ?")
        .bind(&name)
        .bind(&color)
        .bind(&parent_id)
        .bind(&description)
        .bind(id)
        .execute(&db.master_pool)
        .await
        .map_err(|e| e.to_string())?;

    // Ancestors root first, since a mirror can lack the new parent.
    let chain = db.tag_with_ancestors(id).await.map_err(|e| e.to_string())?;
    for lib in db.get_all_libraries().await {
        let mirror = async {
            let mut tx = lib.pool.begin().await?;
            for definition in &chain {
                upsert_tag(&mut *tx, definition).await?;
            }
            tx.commit().await
        };
        if let Err(e) = mirror.await {
            tracing::warn!("Failed to mirror tag {} to library {}: {}", id, lib.id, e);
        }
    }

    Ok(tag_from_row((
        id.to_string(),
        name,
        color,
        parent_id,
        description,
    )))
}

#[tauri::command]
pub async fn delete_tag(id: String, state: State<'_, Arc<AppState>>) -> Result<()> {
    delete_tag_in(&state.db, &id).await
}

/// Deletes the definition from master and every open library, with its memberships.
async fn delete_tag_in(db: &Database, id: &str) -> Result<()> {
    // Mirrors first, each in one transaction, and master last: if a library can't take the
    // delete, master keeps the tag, so the next open doesn't bring a "deleted" tag back.
    for lib in db.get_all_libraries().await {
        let delete = async {
            let mut tx = lib.pool.begin().await?;
            // Memberships are deleted explicitly: not every library connection enforces the cascade.
            sqlx::query("DELETE FROM asset_tags WHERE tag_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM tags WHERE id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await
        };
        delete
            .await
            .map_err(|e| format!("Could not delete the tag from library {}: {}", lib.name, e))?;
    }

    sqlx::query("DELETE FROM tags WHERE id = ?")
        .bind(id)
        .execute(&db.master_pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        insert_asset, insert_tag, memory_database, memory_library, memory_library_pool,
        memory_library_pool_enforcing_foreign_keys,
    };
    use sqlx::{Pool, Sqlite};

    async fn tag_names(pool: &Pool<Sqlite>) -> Vec<String> {
        sqlx::query_scalar("SELECT name FROM tags")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn membership_count(pool: &Pool<Sqlite>) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM asset_tags")
            .fetch_one(pool)
            .await
            .unwrap()
    }

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

    #[tokio::test]
    async fn create_tag_writes_master_and_every_open_library() {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        let db = memory_database(vec![
            memory_library("a", a.clone()),
            memory_library("b", b.clone()),
        ])
        .await;

        let tag = create_tag_in(&db, "rock".into(), None).await.unwrap();

        for pool in [&db.master_pool, &a, &b] {
            assert_eq!(tag_names(pool).await, ["rock"]);
        }
        assert_eq!(tag.name, "rock");
    }

    #[tokio::test]
    async fn delete_tag_removes_it_and_its_memberships_from_master_and_every_library() {
        // Both library pools have foreign keys off, so no cascade removes the memberships.
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        for (pool, asset) in [(&a, "in-a"), (&b, "in-b")] {
            insert_asset(pool, asset).await;
            insert_tag(pool, "t", "rock").await;
            sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES (?, 't')")
                .bind(asset)
                .execute(pool)
                .await
                .unwrap();
        }
        let db = memory_database(vec![
            memory_library("a", a.clone()),
            memory_library("b", b.clone()),
        ])
        .await;
        insert_tag(&db.master_pool, "t", "rock").await;

        delete_tag_in(&db, "t").await.unwrap();

        for pool in [&db.master_pool, &a, &b] {
            assert!(tag_names(pool).await.is_empty());
        }
        assert_eq!(membership_count(&a).await, 0);
        assert_eq!(membership_count(&b).await, 0);
    }

    #[tokio::test]
    async fn delete_tag_that_a_library_cannot_take_is_an_error_and_keeps_master() {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        insert_tag(&a, "t", "rock").await;
        insert_tag(&b, "t", "rock").await;
        let db = memory_database(vec![
            memory_library("a", a.clone()),
            memory_library("b", b.clone()),
        ])
        .await;
        insert_tag(&db.master_pool, "t", "rock").await;
        b.close().await;

        let result = delete_tag_in(&db, "t").await;

        assert!(result.is_err());
        assert_eq!(tag_names(&db.master_pool).await, ["rock"]);
    }

    async fn tag_row(pool: &Pool<Sqlite>, id: &str) -> Option<TagRow> {
        sqlx::query_as("SELECT id, name, color, parent_id, description FROM tags WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    fn patch(json: serde_json::Value) -> TagPatch {
        serde_json::from_value(json).unwrap()
    }

    async fn set_parent(pool: &Pool<Sqlite>, id: &str, parent: &str) {
        sqlx::query("UPDATE tags SET parent_id = ? WHERE id = ?")
            .bind(parent)
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn update_tag_writes_master_then_every_open_library_in_place() {
        // Foreign keys on: a REPLACE would cascade-delete the membership, and a mirror that
        // lacks the new parent would reject the tag.
        let a = memory_library_pool_enforcing_foreign_keys().await;
        let b = memory_library_pool_enforcing_foreign_keys().await;
        for pool in [&a, &b] {
            insert_tag(pool, "t", "rock").await;
        }
        insert_asset(&a, "asset").await;
        sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES ('asset', 't')")
            .execute(&a)
            .await
            .unwrap();
        let db = memory_database(vec![
            memory_library("a", a.clone()),
            memory_library("b", b.clone()),
        ])
        .await;
        insert_tag(&db.master_pool, "t", "rock").await;
        insert_tag(&db.master_pool, "p", "nature").await;

        let tag = update_tag_in(
            &db,
            "t",
            patch(serde_json::json!({
                "name": " stone ",
                "color": "teal",
                "parentId": "p",
                "description": "Rough grey rock surfaces",
            })),
        )
        .await
        .unwrap();

        let expected: TagRow = (
            "t".into(),
            "stone".into(),
            Some("teal".into()),
            Some("p".into()),
            "Rough grey rock surfaces".into(),
        );
        for pool in [&db.master_pool, &a, &b] {
            assert_eq!(tag_row(pool, "t").await, Some(expected.clone()));
            assert_eq!(tag_row(pool, "p").await.unwrap().1, "nature");
        }
        assert_eq!(membership_count(&a).await, 1);
        assert_eq!(tag.name, "stone");
        assert_eq!(tag.description, "Rough grey rock surfaces");
    }

    #[tokio::test]
    async fn update_tag_keeps_fields_left_out_and_clears_fields_sent_as_null() {
        let db = memory_database(vec![]).await;
        insert_tag(&db.master_pool, "p", "nature").await;
        insert_tag(&db.master_pool, "t", "rock").await;
        update_tag_in(
            &db,
            "t",
            patch(serde_json::json!({ "color": "red", "parentId": "p" })),
        )
        .await
        .unwrap();

        update_tag_in(
            &db,
            "t",
            patch(serde_json::json!({ "description": "hint" })),
        )
        .await
        .unwrap();
        let kept = tag_row(&db.master_pool, "t").await.unwrap();
        update_tag_in(
            &db,
            "t",
            patch(serde_json::json!({ "color": null, "parentId": null })),
        )
        .await
        .unwrap();
        let cleared = tag_row(&db.master_pool, "t").await.unwrap();

        assert_eq!(
            kept,
            (
                "t".into(),
                "rock".into(),
                Some("red".into()),
                Some("p".into()),
                "hint".into()
            )
        );
        assert_eq!(
            cleared,
            ("t".into(), "rock".into(), None, None, "hint".into())
        );
    }

    #[tokio::test]
    async fn update_tag_refuses_to_put_a_tag_under_itself_or_its_descendants() {
        let db = memory_database(vec![]).await;
        insert_tag(&db.master_pool, "root", "nature").await;
        insert_tag(&db.master_pool, "child", "rock").await;
        insert_tag(&db.master_pool, "grandchild", "granite").await;
        set_parent(&db.master_pool, "child", "root").await;
        set_parent(&db.master_pool, "grandchild", "child").await;

        for parent in ["root", "child", "grandchild"] {
            let result = update_tag_in(
                &db,
                "root",
                patch(serde_json::json!({ "parentId": parent })),
            )
            .await;
            assert!(result.is_err(), "nesting root under {parent} should fail");
        }
        assert_eq!(tag_row(&db.master_pool, "root").await.unwrap().3, None);
    }

    #[tokio::test]
    async fn update_tag_refuses_a_name_another_tag_has_but_allows_its_own_in_another_case() {
        let a = memory_library_pool().await;
        insert_tag(&a, "t", "rock").await;
        let db = memory_database(vec![memory_library("a", a.clone())]).await;
        insert_tag(&db.master_pool, "t", "rock").await;
        insert_tag(&db.master_pool, "u", "stone").await;

        let collision =
            update_tag_in(&db, "t", patch(serde_json::json!({ "name": "Stone" }))).await;
        assert_eq!(
            collision.unwrap_err(),
            "A tag named \"stone\" already exists"
        );
        assert_eq!(tag_row(&db.master_pool, "t").await.unwrap().1, "rock");
        assert_eq!(tag_row(&a, "t").await.unwrap().1, "rock");

        update_tag_in(&db, "t", patch(serde_json::json!({ "name": "Rock" })))
            .await
            .unwrap();
        assert_eq!(tag_row(&db.master_pool, "t").await.unwrap().1, "Rock");
    }

    #[tokio::test]
    async fn update_tag_refuses_a_name_that_differs_only_by_non_ascii_case() {
        let db = memory_database(vec![]).await;
        insert_tag(&db.master_pool, "t", "rock").await;
        insert_tag(&db.master_pool, "u", "écorce").await;

        let collision =
            update_tag_in(&db, "t", patch(serde_json::json!({ "name": "ÉCORCE" }))).await;

        assert_eq!(
            collision.unwrap_err(),
            "A tag named \"écorce\" already exists"
        );
        assert_eq!(tag_row(&db.master_pool, "t").await.unwrap().1, "rock");
    }

    #[tokio::test]
    async fn update_tag_still_saves_to_master_and_healthy_mirrors_when_one_mirror_refuses() {
        let a = memory_library_pool_enforcing_foreign_keys().await;
        let b = memory_library_pool_enforcing_foreign_keys().await;
        for pool in [&a, &b] {
            insert_tag(pool, "t", "rock").await;
        }
        // b holds another tag with the new name, so it can't take the rename.
        insert_tag(&b, "u", "stone").await;
        let db = memory_database(vec![
            memory_library("a", a.clone()),
            memory_library("b", b.clone()),
        ])
        .await;
        insert_tag(&db.master_pool, "t", "rock").await;
        insert_tag(&db.master_pool, "p", "nature").await;

        let result = update_tag_in(
            &db,
            "t",
            patch(serde_json::json!({ "name": "stone", "parentId": "p", "description": "hint" })),
        )
        .await;

        assert!(result.is_ok());
        let edited: TagRow = (
            "t".into(),
            "stone".into(),
            None,
            Some("p".into()),
            "hint".into(),
        );
        assert_eq!(tag_row(&db.master_pool, "t").await, Some(edited.clone()));
        assert_eq!(tag_row(&a, "t").await, Some(edited));
        assert!(tag_row(&a, "p").await.is_some());
        // b's transaction rolled back: its row is as it was, and it didn't gain the parent.
        assert_eq!(
            tag_row(&b, "t").await,
            Some(("t".into(), "rock".into(), None, None, String::new()))
        );
        assert_eq!(tag_row(&b, "p").await, None);
    }

    #[tokio::test]
    async fn update_tag_refuses_a_missing_parent_an_empty_name_and_an_unknown_tag() {
        let db = memory_database(vec![]).await;
        insert_tag(&db.master_pool, "t", "rock").await;
        let before = tag_row(&db.master_pool, "t").await;

        for (id, json, error) in [
            (
                "t",
                serde_json::json!({ "parentId": "gone" }),
                "The parent tag no longer exists",
            ),
            (
                "t",
                serde_json::json!({ "name": "   " }),
                "A tag needs a name",
            ),
            (
                "gone",
                serde_json::json!({ "name": "stone" }),
                "The tag no longer exists",
            ),
        ] {
            assert_eq!(
                update_tag_in(&db, id, patch(json)).await.unwrap_err(),
                error
            );
            assert_eq!(tag_row(&db.master_pool, "t").await, before);
        }
    }

    #[tokio::test]
    async fn update_tag_stores_a_none_or_empty_color_as_no_color() {
        let db = memory_database(vec![]).await;
        insert_tag(&db.master_pool, "t", "rock").await;

        for color in ["none", ""] {
            update_tag_in(&db, "t", patch(serde_json::json!({ "color": "red" })))
                .await
                .unwrap();
            update_tag_in(&db, "t", patch(serde_json::json!({ "color": color })))
                .await
                .unwrap();
            assert_eq!(tag_row(&db.master_pool, "t").await.unwrap().2, None);
        }
    }
}
