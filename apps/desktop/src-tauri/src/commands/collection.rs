use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use tauri::State;
use uuid::Uuid;

use crate::db::{upsert_collection, CollectionRow, Database};
use crate::models::Collection;
use crate::AppState;

type Result<T> = std::result::Result<T, String>;

#[tauri::command]
pub async fn list_collections(state: State<'_, Arc<AppState>>) -> Result<Vec<Collection>> {
    list_collections_in(&state.db).await
}

/// Definitions come from the master catalog; each collection's assets are the membership rows
/// of every open library.
async fn list_collections_in(db: &Database) -> Result<Vec<Collection>> {
    let rows = sqlx::query_as::<_, CollectionRow>(
        "SELECT id, name, description, parent_id, is_smart, filter_spec, created_at FROM collections ORDER BY created_at ASC"
    )
    .fetch_all(&db.master_pool)
    .await
    .map_err(|e| e.to_string())?;

    let mut members: HashMap<String, Vec<String>> = HashMap::new();
    for lib in db.get_all_libraries().await {
        if let Ok(pairs) = sqlx::query_as::<_, (String, String)>(
            "SELECT collection_id, asset_id FROM collection_assets",
        )
        .fetch_all(&lib.pool)
        .await
        {
            for (collection_id, asset_id) in pairs {
                members.entry(collection_id).or_default().push(asset_id);
            }
        }
    }

    Ok(rows
        .into_iter()
        .map(
            |(id, name, description, parent_id, is_smart, filter_spec, created_at)| Collection {
                asset_ids: members.remove(&id).unwrap_or_default(),
                id,
                name,
                description,
                parent_id,
                is_smart,
                filter_spec,
                created_at,
            },
        )
        .collect())
}

#[tauri::command]
pub async fn create_collection(
    name: String,
    description: Option<String>,
    parent_id: Option<String>,
    is_smart: Option<bool>,
    filter_spec: Option<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<Collection> {
    create_collection_in(
        &state.db,
        name,
        description,
        parent_id,
        is_smart,
        filter_spec,
    )
    .await
}

/// Writes the definition to master, then mirrors it to every open library.
async fn create_collection_in(
    db: &Database,
    name: String,
    description: Option<String>,
    parent_id: Option<String>,
    is_smart: Option<bool>,
    filter_spec: Option<String>,
) -> Result<Collection> {
    let row: CollectionRow = (
        Uuid::new_v4().to_string(),
        name,
        description.unwrap_or_default(),
        parent_id,
        is_smart.unwrap_or(false),
        filter_spec,
        Utc::now().timestamp_millis(),
    );
    sqlx::query(
        "INSERT INTO collections (id, name, description, parent_id, is_smart, filter_spec, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&row.0).bind(&row.1).bind(&row.2).bind(&row.3).bind(row.4).bind(&row.5).bind(row.6)
    .execute(&db.master_pool).await.map_err(|e| e.to_string())?;

    for lib in db.get_all_libraries().await {
        if let Err(e) = upsert_collection(&lib.pool, &row).await {
            tracing::warn!(
                "Failed to mirror collection {} to library {}: {}",
                row.0,
                lib.id,
                e
            );
        }
    }

    let (id, name, description, parent_id, is_smart, filter_spec, created_at) = row;
    Ok(Collection {
        id,
        name,
        description,
        parent_id,
        asset_ids: vec![],
        is_smart,
        filter_spec,
        created_at,
    })
}

#[tauri::command]
pub async fn add_to_collection(
    collection_id: String,
    asset_ids: Vec<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<()> {
    add_assets_to_collection(&state.db, &collection_id, &asset_ids).await
}

/// Each membership row goes to the asset's own library only.
async fn add_assets_to_collection(
    db: &Database,
    collection_id: &str,
    asset_ids: &[String],
) -> Result<()> {
    for aid in asset_ids {
        let Some(lib) = db.library_of_asset(aid).await else {
            tracing::warn!(
                "Asset {} is in no open library, not adding it to a collection",
                aid
            );
            continue;
        };
        sqlx::query(
            "INSERT OR IGNORE INTO collection_assets (collection_id, asset_id) VALUES (?, ?)",
        )
        .bind(collection_id)
        .bind(aid)
        .execute(&lib.pool)
        .await
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn remove_from_collection(
    collection_id: String,
    asset_ids: Vec<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<()> {
    remove_assets_from_collection(&state.db, &collection_id, &asset_ids).await
}

/// Each membership row is removed from the asset's own library.
async fn remove_assets_from_collection(
    db: &Database,
    collection_id: &str,
    asset_ids: &[String],
) -> Result<()> {
    for aid in asset_ids {
        if let Some(lib) = db.library_of_asset(aid).await {
            sqlx::query("DELETE FROM collection_assets WHERE collection_id = ? AND asset_id = ?")
                .bind(collection_id)
                .bind(aid)
                .execute(&lib.pool)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn delete_collection(id: String, state: State<'_, Arc<AppState>>) -> Result<()> {
    delete_collection_in(&state.db, &id).await
}

/// Deletes the collection and its descendants, walked in the master catalog, from master and
/// every open library, with their memberships.
async fn delete_collection_in(db: &Database, id: &str) -> Result<()> {
    // Recursively collect all descendant collection IDs from the catalog
    let mut to_delete = vec![id.to_string()];
    let mut idx = 0;
    while idx < to_delete.len() {
        let cur_id = &to_delete[idx];
        let sub_rows =
            sqlx::query_as::<_, (String,)>("SELECT id FROM collections WHERE parent_id = ?")
                .bind(cur_id)
                .fetch_all(&db.master_pool)
                .await
                .map_err(|e| e.to_string())?;

        for (sub_id,) in sub_rows {
            if !to_delete.contains(&sub_id) {
                to_delete.push(sub_id);
            }
        }
        idx += 1;
    }

    let libs = db.get_all_libraries().await;
    for cid in &to_delete {
        let _ = sqlx::query("DELETE FROM collections WHERE id = ?")
            .bind(cid)
            .execute(&db.master_pool)
            .await;
        for lib in &libs {
            let _ = sqlx::query("DELETE FROM collection_assets WHERE collection_id = ?")
                .bind(cid)
                .execute(&lib.pool)
                .await;
            let _ = sqlx::query("DELETE FROM collections WHERE id = ?")
                .bind(cid)
                .execute(&lib.pool)
                .await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        insert_asset, insert_collection, memory_database, memory_library, memory_library_pool,
    };

    async fn collected(pool: &sqlx::Pool<sqlx::Sqlite>) -> Vec<String> {
        sqlx::query_scalar("SELECT asset_id FROM collection_assets ORDER BY asset_id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn collecting_assets_writes_each_membership_only_to_the_assets_own_library() {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        insert_asset(&a, "in-a").await;
        insert_asset(&b, "in-b").await;
        let db = memory_database(vec![
            memory_library("a", a.clone()),
            memory_library("b", b.clone()),
        ])
        .await;
        for pool in [&db.master_pool, &a, &b] {
            insert_collection(pool, "c", "props", None).await;
        }

        add_assets_to_collection(&db, "c", &["in-a".into(), "in-b".into()])
            .await
            .unwrap();

        assert_eq!(collected(&a).await, ["in-a"]);
        assert_eq!(collected(&b).await, ["in-b"]);
        assert!(collected(&db.master_pool).await.is_empty());
    }

    #[tokio::test]
    async fn list_collections_reads_master_definitions_and_every_librarys_memberships() {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        for (pool, asset) in [(&a, "in-a"), (&b, "in-b")] {
            insert_asset(pool, asset).await;
            insert_collection(pool, "c", "mirror name", None).await;
            sqlx::query("INSERT INTO collection_assets (collection_id, asset_id) VALUES ('c', ?)")
                .bind(asset)
                .execute(pool)
                .await
                .unwrap();
        }
        let db = memory_database(vec![memory_library("a", a), memory_library("b", b)]).await;
        insert_collection(&db.master_pool, "c", "master name", None).await;

        let collections = list_collections_in(&db).await.unwrap();

        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].name, "master name");
        assert_eq!(collections[0].asset_ids, ["in-a", "in-b"]);
    }

    async fn collection_ids(pool: &sqlx::Pool<sqlx::Sqlite>) -> Vec<String> {
        sqlx::query_scalar("SELECT id FROM collections ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn collect(pool: &sqlx::Pool<sqlx::Sqlite>, collection_id: &str, asset_id: &str) {
        sqlx::query("INSERT INTO collection_assets (collection_id, asset_id) VALUES (?, ?)")
            .bind(collection_id)
            .bind(asset_id)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_collection_writes_master_and_every_open_library() {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        let db = memory_database(vec![
            memory_library("a", a.clone()),
            memory_library("b", b.clone()),
        ])
        .await;

        let created = create_collection_in(&db, "props".into(), None, None, None, None)
            .await
            .unwrap();

        for pool in [&db.master_pool, &a, &b] {
            assert_eq!(collection_ids(pool).await, [created.id.as_str()]);
        }
    }

    #[tokio::test]
    async fn delete_collection_removes_its_master_descendants_and_memberships_everywhere() {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        for (pool, asset) in [(&a, "in-a"), (&b, "in-b")] {
            insert_asset(pool, asset).await;
            // The mirrors are stale: only master knows "child" sits under "parent".
            insert_collection(pool, "parent", "parent", None).await;
            insert_collection(pool, "child", "child", None).await;
            collect(pool, "parent", asset).await;
            collect(pool, "child", asset).await;
        }
        let db = memory_database(vec![
            memory_library("a", a.clone()),
            memory_library("b", b.clone()),
        ])
        .await;
        insert_collection(&db.master_pool, "parent", "parent", None).await;
        insert_collection(&db.master_pool, "child", "child", Some("parent")).await;

        delete_collection_in(&db, "parent").await.unwrap();

        for pool in [&db.master_pool, &a, &b] {
            assert!(collection_ids(pool).await.is_empty());
            assert!(collected(pool).await.is_empty());
        }
    }

    #[tokio::test]
    async fn remove_from_collection_removes_only_the_named_assets_membership() {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        for (pool, asset) in [(&a, "in-a"), (&b, "in-b")] {
            insert_asset(pool, asset).await;
            insert_collection(pool, "c", "props", None).await;
            collect(pool, "c", asset).await;
        }
        let db = memory_database(vec![
            memory_library("a", a.clone()),
            memory_library("b", b.clone()),
        ])
        .await;

        remove_assets_from_collection(&db, "c", &["in-a".into()])
            .await
            .unwrap();

        assert!(collected(&a).await.is_empty());
        assert_eq!(collected(&b).await, ["in-b"]);
    }

    #[tokio::test]
    async fn collecting_an_asset_in_no_open_library_writes_nothing() {
        let a = memory_library_pool().await;
        insert_collection(&a, "c", "props", None).await;
        let db = memory_database(vec![memory_library("a", a.clone())]).await;

        add_assets_to_collection(&db, "c", &["nowhere".into()])
            .await
            .unwrap();

        assert!(collected(&a).await.is_empty());
        assert!(collected(&db.master_pool).await.is_empty());
    }
}
