//! Run state in the master DB, and the library writes of the write protocol
//! (docs/plans/ai-auto-tagging/plan.md, "Write protocol").

use serde::Serialize;
use sqlx::{Pool, Sqlite};
use std::collections::{BTreeMap, HashSet};

use crate::db::{upsert_tag, Database, LibraryHandle};

/// Rows per transaction, the repository's batch size.
const BATCH: usize = 200;

/// The run tables. `tag_run_applied` has no foreign key on its run: its `pending` rows outlive
/// a cleared run until replay applies them.
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tag_runs (
    id            TEXT PRIMARY KEY,
    label         TEXT NOT NULL,
    provider      TEXT NOT NULL,
    access        TEXT,
    model         TEXT,
    mode          TEXT NOT NULL,
    status        TEXT NOT NULL,
    pause_reason  TEXT,
    inline        INTEGER NOT NULL DEFAULT 0,
    options       TEXT NOT NULL DEFAULT '{}',
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd      REAL NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    finished_at   INTEGER
);
CREATE TABLE IF NOT EXISTS tag_run_items (
    run_id     TEXT NOT NULL REFERENCES tag_runs(id) ON DELETE CASCADE,
    asset_id   TEXT NOT NULL,
    library_id TEXT NOT NULL,
    status     TEXT NOT NULL DEFAULT 'queued',
    attempts   INTEGER NOT NULL DEFAULT 0,
    error      TEXT,
    result     TEXT,
    PRIMARY KEY (run_id, asset_id)
);
CREATE INDEX IF NOT EXISTS idx_tag_run_items_status ON tag_run_items(run_id, status);
CREATE TABLE IF NOT EXISTS tag_run_proposals (
    id              TEXT PRIMARY KEY,
    run_id          TEXT NOT NULL REFERENCES tag_runs(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    parent_id       TEXT,
    status          TEXT NOT NULL DEFAULT 'pending',
    final_tag_id    TEXT,
    created_tag     INTEGER NOT NULL DEFAULT 0,
    UNIQUE (run_id, normalized_name)
);
CREATE TABLE IF NOT EXISTS tag_run_proposal_assets (
    proposal_id TEXT NOT NULL REFERENCES tag_run_proposals(id) ON DELETE CASCADE,
    asset_id    TEXT NOT NULL,
    included    INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (proposal_id, asset_id)
);
CREATE TABLE IF NOT EXISTS tag_run_applied (
    run_id     TEXT NOT NULL,
    asset_id   TEXT NOT NULL,
    library_id TEXT NOT NULL,
    tag_id     TEXT NOT NULL,
    state      TEXT NOT NULL DEFAULT 'pending',
    PRIMARY KEY (run_id, asset_id, tag_id)
);
CREATE INDEX IF NOT EXISTS idx_tag_run_applied_state ON tag_run_applied(state, library_id);
"#;

/// A run as the status bar and the Runs panel show it.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub id: String,
    pub label: String,
    pub provider: String,
    #[sqlx(skip)]
    pub provider_label: String,
    pub model: Option<String>,
    pub mode: String,
    pub status: String,
    pub pause_reason: Option<String>,
    pub total: i64,
    pub done: i64,
    pub failed: i64,
    pub running: i64,
    pub queued: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Every run, newest first.
pub async fn list_runs(master: &Pool<Sqlite>) -> sqlx::Result<Vec<RunSummary>> {
    sqlx::query_as(
        "SELECT r.id, r.label, r.provider, r.model, r.mode, r.status, r.pause_reason,
                COUNT(i.asset_id) AS total,
                COALESCE(SUM(i.status = 'done'), 0) AS done,
                COALESCE(SUM(i.status = 'failed'), 0) AS failed,
                COALESCE(SUM(i.status = 'running'), 0) AS running,
                COALESCE(SUM(i.status = 'queued'), 0) AS queued,
                r.input_tokens, r.output_tokens, r.cost_usd, r.created_at, r.finished_at
         FROM tag_runs r LEFT JOIN tag_run_items i ON i.run_id = r.id
         GROUP BY r.id
         ORDER BY r.created_at DESC, r.id",
    )
    .fetch_all(master)
    .await
}

/// Which open library holds each asset, in the order given. Assets no open library holds are
/// left out.
pub async fn locate_assets(
    db: &Database,
    asset_ids: &[String],
) -> sqlx::Result<Vec<(String, String)>> {
    let mut library_of = std::collections::HashMap::new();
    for lib in db.get_all_libraries().await {
        for chunk in asset_ids.chunks(BATCH) {
            let sql = format!(
                "SELECT id FROM assets WHERE id IN ({})",
                vec!["?"; chunk.len()].join(", ")
            );
            let mut query = sqlx::query_scalar::<_, String>(&sql);
            for id in chunk {
                query = query.bind(id);
            }
            for id in query.fetch_all(&lib.pool).await? {
                library_of.entry(id).or_insert_with(|| lib.id.clone());
            }
        }
    }
    let mut seen = HashSet::new();
    Ok(asset_ids
        .iter()
        .filter(|id| seen.insert(id.as_str()))
        .filter_map(|id| library_of.get(id).map(|lib| (id.clone(), lib.clone())))
        .collect())
}

/// A library's assets with no tag, or all of them with `all_assets`, in file path order.
pub async fn library_assets(lib: &Pool<Sqlite>, all_assets: bool) -> sqlx::Result<Vec<String>> {
    let sql = if all_assets {
        "SELECT id FROM assets ORDER BY file_path"
    } else {
        "SELECT id FROM assets a
         WHERE NOT EXISTS (SELECT 1 FROM asset_tags t WHERE t.asset_id = a.id)
         ORDER BY file_path"
    };
    sqlx::query_scalar(sql).fetch_all(lib).await
}

pub struct NewRun<'a> {
    pub id: &'a str,
    pub label: &'a str,
    pub provider: &'a str,
    pub model: Option<&'a str>,
    pub mode: &'a str,
    pub options: serde_json::Value,
}

/// Inserts the run and its queued items. The run is `queued` while its items go in, in batches,
/// and `running` once they all have, so the scheduler never sees half a run.
pub async fn create_run(
    master: &Pool<Sqlite>,
    run: NewRun<'_>,
    items: &[(String, String)],
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO tag_runs (id, label, provider, model, mode, status, options, created_at)
         VALUES (?, ?, ?, ?, ?, 'queued', ?, ?)",
    )
    .bind(run.id)
    .bind(run.label)
    .bind(run.provider)
    .bind(run.model)
    .bind(run.mode)
    .bind(run.options.to_string())
    .bind(now_ms())
    .execute(master)
    .await?;
    for chunk in items.chunks(BATCH) {
        let mut tx = master.begin().await?;
        for (asset_id, library_id) in chunk {
            sqlx::query(
                "INSERT INTO tag_run_items (run_id, asset_id, library_id) VALUES (?, ?, ?)",
            )
            .bind(run.id)
            .bind(asset_id)
            .bind(library_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }
    sqlx::query("UPDATE tag_runs SET status = 'running' WHERE id = ?")
        .bind(run.id)
        .execute(master)
        .await?;
    Ok(())
}

/// A claimed item, now `running`.
#[derive(Debug, Clone)]
pub struct Item {
    pub run_id: String,
    pub asset_id: String,
    pub library_id: String,
    pub mode: String,
    pub options: serde_json::Value,
}

/// The running runs of `provider` that still have queued items, oldest first.
pub async fn runs_with_work(master: &Pool<Sqlite>, provider: &str) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT r.id FROM tag_runs r
         WHERE r.provider = ? AND r.status = 'running'
           AND EXISTS (SELECT 1 FROM tag_run_items i WHERE i.run_id = r.id AND i.status = 'queued')
         ORDER BY r.created_at, r.id",
    )
    .bind(provider)
    .fetch_all(master)
    .await
}

/// Marks the run's next queued item `running` and returns it.
pub async fn claim_item(master: &Pool<Sqlite>, run_id: &str) -> sqlx::Result<Option<Item>> {
    let claimed: Option<(String, String)> = sqlx::query_as(
        "UPDATE tag_run_items SET status = 'running', attempts = attempts + 1
         WHERE rowid = (SELECT rowid FROM tag_run_items
                        WHERE run_id = ? AND status = 'queued' ORDER BY rowid LIMIT 1)
         RETURNING asset_id, library_id",
    )
    .bind(run_id)
    .fetch_optional(master)
    .await?;
    let Some((asset_id, library_id)) = claimed else {
        return Ok(None);
    };
    let (mode, options): (String, String) =
        sqlx::query_as("SELECT mode, options FROM tag_runs WHERE id = ?")
            .bind(run_id)
            .fetch_one(master)
            .await?;
    Ok(Some(Item {
        run_id: run_id.to_string(),
        asset_id,
        library_id,
        mode,
        options: serde_json::from_str(&options).unwrap_or_default(),
    }))
}

pub async fn fail_item(master: &Pool<Sqlite>, item: &Item, error: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE tag_run_items SET status = 'failed', error = ? WHERE run_id = ? AND asset_id = ?",
    )
    .bind(error)
    .bind(&item.run_id)
    .bind(&item.asset_id)
    .execute(master)
    .await
    .map(|_| ())
}

/// Ends a running run once none of its items is queued or running. Returns whether it ended.
pub async fn finish_run_if_complete(master: &Pool<Sqlite>, run_id: &str) -> sqlx::Result<bool> {
    let ended = sqlx::query(
        "UPDATE tag_runs SET status = 'done', finished_at = ?
         WHERE id = ? AND status = 'running'
           AND NOT EXISTS (SELECT 1 FROM tag_run_items
                           WHERE run_id = ? AND status IN ('queued', 'running'))",
    )
    .bind(now_ms())
    .bind(run_id)
    .bind(run_id)
    .execute(master)
    .await?;
    Ok(ended.rows_affected() > 0)
}

/// Items left `running` by a previous session go back to the queue.
pub async fn requeue_running_items(master: &Pool<Sqlite>) -> sqlx::Result<()> {
    sqlx::query("UPDATE tag_run_items SET status = 'queued' WHERE status = 'running'")
        .execute(master)
        .await
        .map(|_| ())
}

/// Write protocol step 1, in one master transaction: a `pending` row for every pair the
/// library lacks, then the item `done` with its result, and the run's usage.
pub async fn record_intent(
    master: &Pool<Sqlite>,
    item: &Item,
    new_tag_ids: &[String],
    result: &serde_json::Value,
    usage: &super::provider::Usage,
) -> sqlx::Result<()> {
    let mut tx = master.begin().await?;
    for tag_id in new_tag_ids {
        sqlx::query(
            "INSERT OR IGNORE INTO tag_run_applied (run_id, asset_id, library_id, tag_id, state)
             VALUES (?, ?, ?, ?, 'pending')",
        )
        .bind(&item.run_id)
        .bind(&item.asset_id)
        .bind(&item.library_id)
        .bind(tag_id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE tag_run_items SET status = 'done', error = NULL, result = ?
         WHERE run_id = ? AND asset_id = ?",
    )
    .bind(result.to_string())
    .bind(&item.run_id)
    .bind(&item.asset_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE tag_runs SET input_tokens = input_tokens + ?, output_tokens = output_tokens + ?,
                             cost_usd = cost_usd + ?
         WHERE id = ?",
    )
    .bind(usage.input_tokens)
    .bind(usage.output_tokens)
    .bind(usage.cost_usd)
    .bind(&item.run_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// A `tag_run_applied` row still waiting for its library write.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, sqlx::FromRow)]
pub struct PendingPair {
    pub run_id: String,
    pub asset_id: String,
    pub library_id: String,
    pub tag_id: String,
    /// The run no longer exists: the row is deleted once applied.
    pub run_cleared: bool,
}

/// One item's pending rows.
pub async fn pending_pairs_of(
    master: &Pool<Sqlite>,
    item: &Item,
) -> sqlx::Result<Vec<PendingPair>> {
    sqlx::query_as(
        "SELECT run_id, asset_id, library_id, tag_id, 0 AS run_cleared FROM tag_run_applied
         WHERE run_id = ? AND asset_id = ? AND state = 'pending'",
    )
    .bind(&item.run_id)
    .bind(&item.asset_id)
    .fetch_all(master)
    .await
}

/// Who runs steps 3 and 4: the item that just recorded the pairs in step 1, or replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applier {
    Item,
    Replay,
}

/// What step 3 did with one pair.
enum Outcome {
    Inserted,
    AlreadyThere,
    /// The tag left the catalog meanwhile: nothing to write.
    Dropped,
}

/// Write protocol steps 3 and 4 for pairs of one open library, in batches: `INSERT OR IGNORE`
/// into its `asset_tags` (with the tag's definition and ancestors, which a mirror can lack),
/// then mark the rows `applied`. Returns the assets whose tags changed.
///
/// - A pair whose tag left the catalog is dropped, its row deleted.
/// - A pair whose definition the mirror can't take (another mirror tag has its name, which
///   `sync_open_libraries` leaves as it is) stays `pending`, and the rest of the batch goes on.
/// - On the item path, a pair the library already holds was added after step 1 by someone
///   else (you, or an overlapping run), so its row is deleted and undo never removes it.
///   Replay can't tell that from its own step 3 committed before a crash, so it marks the row
///   `applied`.
pub async fn apply_pairs(
    db: &Database,
    lib: &LibraryHandle,
    pairs: &[PendingPair],
    applier: Applier,
) -> sqlx::Result<Vec<String>> {
    let mut changed = Vec::new();
    for chunk in pairs.chunks(BATCH) {
        let mut outcomes = Vec::new();
        let mut tx = lib.pool.begin().await?;
        'pairs: for pair in chunk {
            let chain = db.tag_with_ancestors(&pair.tag_id).await?;
            if chain.is_empty() {
                outcomes.push((pair, Outcome::Dropped));
                continue;
            }
            for definition in &chain {
                // A failed statement rolls back only itself; the transaction goes on.
                if let Err(e) = upsert_tag(&mut *tx, definition).await {
                    tracing::warn!(
                        "Tag {} for asset {} waits in library {}: its mirror can't take {:?}: {e}",
                        pair.tag_id,
                        pair.asset_id,
                        lib.id,
                        definition.1
                    );
                    continue 'pairs;
                }
            }
            let inserted =
                sqlx::query("INSERT OR IGNORE INTO asset_tags (asset_id, tag_id) VALUES (?, ?)")
                    .bind(&pair.asset_id)
                    .bind(&pair.tag_id)
                    .execute(&mut *tx)
                    .await?;
            if inserted.rows_affected() > 0 {
                changed.push(pair.asset_id.clone());
                outcomes.push((pair, Outcome::Inserted));
            } else {
                outcomes.push((pair, Outcome::AlreadyThere));
            }
        }
        tx.commit().await?;

        let mut tx = db.master_pool.begin().await?;
        for (pair, outcome) in &outcomes {
            let keep = match outcome {
                Outcome::Dropped => false,
                Outcome::AlreadyThere if applier == Applier::Item => false,
                _ => !pair.run_cleared,
            };
            let sql = if keep {
                "UPDATE tag_run_applied SET state = 'applied'
                 WHERE run_id = ? AND asset_id = ? AND tag_id = ?"
            } else {
                "DELETE FROM tag_run_applied WHERE run_id = ? AND asset_id = ? AND tag_id = ?"
            };
            sqlx::query(sql)
                .bind(&pair.run_id)
                .bind(&pair.asset_id)
                .bind(&pair.tag_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
    changed.dedup();
    Ok(changed)
}

/// Replays every `pending` row whose library is open (steps 3 and 4, both idempotent). Rows of
/// runs being undone are skipped: they wait to be removed, not applied. Rows for an offline
/// library wait for it. Returns the assets whose tags changed.
pub async fn replay_pending(db: &Database) -> sqlx::Result<Vec<String>> {
    let rows: Vec<PendingPair> = sqlx::query_as(
        "SELECT a.run_id, a.asset_id, a.library_id, a.tag_id, (r.id IS NULL) AS run_cleared
         FROM tag_run_applied a LEFT JOIN tag_runs r ON r.id = a.run_id
         WHERE a.state = 'pending'
           AND (r.status IS NULL OR r.status NOT IN ('undoing', 'undo_partial', 'undone'))
         ORDER BY a.library_id, a.asset_id, a.tag_id",
    )
    .fetch_all(&db.master_pool)
    .await?;
    let mut by_library: BTreeMap<String, Vec<PendingPair>> = BTreeMap::new();
    for row in rows {
        by_library
            .entry(row.library_id.clone())
            .or_default()
            .push(row);
    }
    let mut changed = Vec::new();
    for (library_id, pairs) in by_library {
        let Some(lib) = db.get_library_handle(&library_id).await else {
            continue;
        };
        // One library's failure (a NAS lock timeout) leaves its rows for the next replay.
        match apply_pairs(db, &lib, &pairs, Applier::Replay).await {
            Ok(assets) => changed.extend(assets),
            Err(e) => tracing::warn!("Pending tags for library {library_id} wait: {e}"),
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        insert_asset, insert_tag, memory_database, memory_library, memory_library_pool,
    };

    async fn asset_tags(pool: &Pool<Sqlite>) -> Vec<(String, String)> {
        sqlx::query_as("SELECT asset_id, tag_id FROM asset_tags ORDER BY asset_id, tag_id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn insert_run(db: &Database, id: &str, status: &str) {
        sqlx::query(
            "INSERT INTO tag_runs (id, label, provider, mode, status, created_at)
             VALUES (?, 'run', 'fake', 'existing_only', ?, 0)",
        )
        .bind(id)
        .bind(status)
        .execute(&db.master_pool)
        .await
        .unwrap();
    }

    async fn insert_pending(db: &Database, run: &str, asset: &str, library: &str, tag: &str) {
        sqlx::query(
            "INSERT INTO tag_run_applied (run_id, asset_id, library_id, tag_id) VALUES (?, ?, ?, ?)",
        )
        .bind(run)
        .bind(asset)
        .bind(library)
        .bind(tag)
        .execute(&db.master_pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn replay_applies_pending_rows_except_those_of_runs_being_undone() {
        let lib_pool = memory_library_pool().await;
        insert_asset(&lib_pool, "a1").await;
        let db = memory_database(vec![memory_library("lib", lib_pool.clone())]).await;
        insert_tag(&db.master_pool, "t1", "Rock").await;
        insert_tag(&db.master_pool, "t2", "Tree").await;
        insert_run(&db, "done-run", "done").await;
        insert_run(&db, "undone-run", "undo_partial").await;
        insert_pending(&db, "done-run", "a1", "lib", "t1").await;
        insert_pending(&db, "undone-run", "a1", "lib", "t2").await;
        insert_pending(&db, "cleared-run", "a1", "lib", "t2").await;
        insert_pending(&db, "done-run", "a9", "offline-lib", "t1").await;

        let changed = replay_pending(&db).await.unwrap();

        assert_eq!(changed, ["a1"]);
        assert_eq!(
            asset_tags(&lib_pool).await,
            [("a1".into(), "t1".into()), ("a1".into(), "t2".into())]
        );
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT run_id, tag_id, state FROM tag_run_applied ORDER BY run_id, asset_id",
        )
        .fetch_all(&db.master_pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [
                ("done-run".into(), "t1".into(), "applied".into()),
                ("done-run".into(), "t1".into(), "pending".into()),
                ("undone-run".into(), "t2".into(), "pending".into()),
            ],
            "the cleared run's row is gone once applied; offline and undo rows wait"
        );
    }

    async fn applied_rows(db: &Database) -> Vec<(String, String, String)> {
        sqlx::query_as(
            "SELECT asset_id, tag_id, state FROM tag_run_applied ORDER BY asset_id, tag_id",
        )
        .fetch_all(&db.master_pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_name_colliding_mirror_tag_leaves_only_its_pair_pending() {
        let lib_pool = memory_library_pool().await;
        insert_asset(&lib_pool, "a1").await;
        insert_tag(&lib_pool, "t-mirror-rock", "Rock").await;
        let db = memory_database(vec![memory_library("lib", lib_pool.clone())]).await;
        insert_tag(&db.master_pool, "t-rock", "Rock").await;
        insert_tag(&db.master_pool, "t-tree", "Tree").await;
        insert_run(&db, "run", "done").await;
        insert_pending(&db, "run", "a1", "lib", "t-rock").await;
        insert_pending(&db, "run", "a1", "lib", "t-tree").await;

        for _ in 0..2 {
            assert!(replay_pending(&db).await.unwrap().len() <= 1);
            assert_eq!(
                asset_tags(&lib_pool).await,
                [("a1".into(), "t-tree".into())],
                "the other pair in the batch still lands"
            );
            assert_eq!(
                applied_rows(&db).await,
                [
                    ("a1".into(), "t-rock".into(), "pending".into()),
                    ("a1".into(), "t-tree".into(), "applied".into()),
                ],
                "the colliding pair waits, on every replay"
            );
        }
    }

    #[tokio::test]
    async fn replay_moves_on_when_a_library_fails() {
        let broken = memory_library_pool().await;
        let fine = memory_library_pool().await;
        insert_asset(&fine, "b1").await;
        let db = memory_database(vec![
            memory_library("a-lib", broken.clone()),
            memory_library("b-lib", fine.clone()),
        ])
        .await;
        insert_tag(&db.master_pool, "t1", "Rock").await;
        insert_run(&db, "run", "done").await;
        insert_pending(&db, "run", "a1", "a-lib", "t1").await;
        insert_pending(&db, "run", "b1", "b-lib", "t1").await;
        broken.close().await;

        let changed = replay_pending(&db).await.unwrap();

        assert_eq!(changed, ["b1"]);
        assert_eq!(
            applied_rows(&db).await,
            [
                ("a1".into(), "t1".into(), "pending".into()),
                ("b1".into(), "t1".into(), "applied".into()),
            ]
        );
    }

    #[tokio::test]
    async fn a_pair_someone_else_added_after_step_one_is_not_claimed_by_the_item() {
        let lib_pool = memory_library_pool().await;
        insert_asset(&lib_pool, "a1").await;
        insert_asset(&lib_pool, "a2").await;
        let db = memory_database(vec![memory_library("lib", lib_pool.clone())]).await;
        insert_tag(&db.master_pool, "t1", "Rock").await;
        insert_run(&db, "run", "running").await;
        insert_pending(&db, "run", "a1", "lib", "t1").await;
        insert_pending(&db, "run", "a2", "lib", "t1").await;
        // Between step 1 and step 3, the pair lands some other way: by hand, or another run.
        for asset in ["a1", "a2"] {
            sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES (?, 't1')")
                .bind(asset)
                .execute(&lib_pool)
                .await
                .unwrap();
        }
        let lib = db.get_library_handle("lib").await.unwrap();
        let pending: Vec<PendingPair> = sqlx::query_as(
            "SELECT run_id, asset_id, library_id, tag_id, 0 AS run_cleared FROM tag_run_applied
             ORDER BY asset_id",
        )
        .fetch_all(&db.master_pool)
        .await
        .unwrap();

        let item = apply_pairs(&db, &lib, &pending[..1], Applier::Item)
            .await
            .unwrap();
        // Replay can't tell this from its own library write before a crash, so it keeps it.
        let replay = apply_pairs(&db, &lib, &pending[1..], Applier::Replay)
            .await
            .unwrap();

        assert!(item.is_empty() && replay.is_empty());
        assert_eq!(
            applied_rows(&db).await,
            [("a2".into(), "t1".into(), "applied".into())],
            "the item path deletes a1's row, so undo never removes a tag it didn't add"
        );
        assert_eq!(asset_tags(&lib_pool).await.len(), 2);
    }
}
