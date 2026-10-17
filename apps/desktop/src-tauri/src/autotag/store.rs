//! Run state in the master DB, and the library writes of the write protocol
//! (docs/plans/ai-auto-tagging/plan.md, "Write protocol").

use serde::Serialize;
use sqlx::{Pool, Sqlite};
use std::collections::{BTreeMap, HashSet};

use super::provider::ProposedTag;
use super::vocabulary::normalize_name;
use crate::db::{create_master_tag, upsert_tag, Database, LibraryHandle};

/// Rows per transaction, the repository's batch size.
pub const BATCH: usize = 200;

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
    notice        TEXT,
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

/// Columns added to the run tables after they first shipped, for master DBs made before.
/// Each fails harmlessly once its column exists.
pub const ADDED_COLUMNS: [&str; 1] = ["ALTER TABLE tag_runs ADD COLUMN notice TEXT"];

/// A run as the status bar and the Runs panel show it.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub id: String,
    pub label: String,
    pub provider: String,
    #[sqlx(skip)]
    pub provider_label: String,
    /// "api_key" or "claude_plan" for Anthropic runs, none for providers with one access.
    pub access: Option<String>,
    pub model: Option<String>,
    pub mode: String,
    pub status: String,
    pub pause_reason: Option<String>,
    /// What the provider last reported without pausing, such as Claude plan usage.
    pub notice: Option<String>,
    /// Started by "Suggest tags" on one asset.
    pub inline: bool,
    pub total: i64,
    pub done: i64,
    pub failed: i64,
    pub skipped: i64,
    pub running: i64,
    pub queued: i64,
    /// New tag names waiting for review.
    pub pending_proposals: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

const RUN_SUMMARY: &str =
    "SELECT r.id, r.label, r.provider, r.access, r.model, r.mode, r.status, r.pause_reason,
            r.notice, r.inline,
            COUNT(i.asset_id) AS total,
            COALESCE(SUM(i.status = 'done'), 0) AS done,
            COALESCE(SUM(i.status = 'failed'), 0) AS failed,
            COALESCE(SUM(i.status = 'skipped'), 0) AS skipped,
            COALESCE(SUM(i.status = 'running'), 0) AS running,
            COALESCE(SUM(i.status = 'queued'), 0) AS queued,
            (SELECT COUNT(*) FROM tag_run_proposals p
             WHERE p.run_id = r.id AND p.status = 'pending') AS pending_proposals,
            r.input_tokens, r.output_tokens, r.cost_usd, r.created_at, r.finished_at
     FROM tag_runs r LEFT JOIN tag_run_items i ON i.run_id = r.id";

/// Every run, newest first.
pub async fn list_runs(master: &Pool<Sqlite>) -> sqlx::Result<Vec<RunSummary>> {
    sqlx::query_as(&format!(
        "{RUN_SUMMARY} GROUP BY r.id ORDER BY r.created_at DESC, r.id"
    ))
    .fetch_all(master)
    .await
}

pub async fn run_summary(master: &Pool<Sqlite>, run_id: &str) -> sqlx::Result<Option<RunSummary>> {
    sqlx::query_as(&format!("{RUN_SUMMARY} WHERE r.id = ? GROUP BY r.id"))
        .bind(run_id)
        .fetch_optional(master)
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

/// The "Suggest tags" runs of one asset, newest first, each with its item's error if it failed.
pub async fn inline_runs_of(
    master: &Pool<Sqlite>,
    asset_id: &str,
) -> sqlx::Result<Vec<(String, Option<String>)>> {
    sqlx::query_as(
        "SELECT r.id, i.error FROM tag_runs r JOIN tag_run_items i ON i.run_id = r.id
         WHERE r.inline = 1 AND i.asset_id = ?
         ORDER BY r.created_at DESC, r.rowid DESC",
    )
    .bind(asset_id)
    .fetch_all(master)
    .await
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
    pub access: Option<&'a str>,
    pub model: Option<&'a str>,
    pub mode: &'a str,
    pub options: serde_json::Value,
    /// Started by "Suggest tags" on one asset.
    pub inline: bool,
}

/// Inserts the run and its queued items. The run is `queued` while its items go in, in batches,
/// and `running` once they all have, so the scheduler never sees half a run. A run paused or
/// cancelled meanwhile stays so.
pub async fn create_run(
    master: &Pool<Sqlite>,
    run: NewRun<'_>,
    items: &[(String, String)],
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO tag_runs (id, label, provider, access, model, mode, status, options,
                               inline, created_at)
         VALUES (?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?)",
    )
    .bind(run.id)
    .bind(run.label)
    .bind(run.provider)
    .bind(run.access)
    .bind(run.model)
    .bind(run.mode)
    .bind(run.options.to_string())
    .bind(run.inline)
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
    finish_creating(master, run.id).await
}

/// Ends `create_run`: the run becomes `running`, unless it was paused or cancelled while its
/// items went in. A cancel that landed between batches skipped only the items already in, so
/// the later ones are skipped here rather than left queued in a run nobody schedules.
async fn finish_creating(master: &Pool<Sqlite>, run_id: &str) -> sqlx::Result<()> {
    let mut tx = master.begin().await?;
    sqlx::query("UPDATE tag_runs SET status = 'running' WHERE id = ? AND status = 'queued'")
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE tag_run_items SET status = 'skipped'
         WHERE run_id = ?1 AND status = 'queued'
           AND (SELECT status FROM tag_runs WHERE id = ?1) = 'cancelled'",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// A claimed item, now `running`.
#[derive(Debug, Clone)]
pub struct Item {
    pub run_id: String,
    pub asset_id: String,
    pub library_id: String,
    pub mode: String,
    pub access: Option<String>,
    pub model: Option<String>,
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
    let (mode, access, model, options): (String, Option<String>, Option<String>, String) =
        sqlx::query_as("SELECT mode, access, model, options FROM tag_runs WHERE id = ?")
            .bind(run_id)
            .fetch_one(master)
            .await?;
    Ok(Some(Item {
        run_id: run_id.to_string(),
        asset_id,
        library_id,
        mode,
        access,
        model,
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

/// Ends a running run once none of its items is queued or running: `awaiting_review` when it
/// has new tag names to review, else `done`. Returns the status it ended with, if it ended.
pub async fn finish_run_if_complete(
    master: &Pool<Sqlite>,
    run_id: &str,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "UPDATE tag_runs
         SET status = CASE WHEN EXISTS (SELECT 1 FROM tag_run_proposals
                                        WHERE run_id = ?1 AND status = 'pending')
                           THEN 'awaiting_review' ELSE 'done' END,
             finished_at = ?2
         WHERE id = ?1 AND status = 'running'
           AND NOT EXISTS (SELECT 1 FROM tag_run_items
                           WHERE run_id = ?1 AND status IN ('queued', 'running'))
         RETURNING status",
    )
    .bind(run_id)
    .bind(now_ms())
    .fetch_optional(master)
    .await
}

/// The `pause_reason` of every run a previous session left unfinished.
pub const APP_RESTARTED: &str = "app restarted";
/// How the `pause_reason` of a run a previous session left half-created begins.
pub const APP_RESTARTED_WHILE_STARTING: &str = "app restarted while the run was starting";

/// At startup, in one transaction: every `queued` or `running` run becomes `paused` with "app
/// restarted", and every item left `running` goes back to the queue, or is `skipped` when its
/// run was cancelled while the item was in flight.
///
/// A run still `queued` was being created, its items going in by batches, so it may hold only
/// some of the assets asked for. Its reason says so and how many it holds, so Resume finishing
/// it never passes for the whole run.
pub async fn recover_after_restart(master: &Pool<Sqlite>) -> sqlx::Result<()> {
    let mut tx = master.begin().await?;
    sqlx::query(
        "UPDATE tag_runs
         SET status = 'paused',
             pause_reason = ?1 || ': it holds only ' || n || CASE n WHEN 1 THEN ' asset' ELSE ' assets' END
                            || ', and any others were never added'
         FROM (SELECT r.id AS run_id, COUNT(i.asset_id) AS n
               FROM tag_runs r LEFT JOIN tag_run_items i ON i.run_id = r.id
               WHERE r.status = 'queued' GROUP BY r.id) AS counts
         WHERE tag_runs.id = counts.run_id",
    )
    .bind(APP_RESTARTED_WHILE_STARTING)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE tag_runs SET status = 'paused', pause_reason = ? WHERE status = 'running'")
        .bind(APP_RESTARTED)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE tag_run_items
         SET status = CASE WHEN (SELECT status FROM tag_runs WHERE id = run_id) = 'cancelled'
                           THEN 'skipped' ELSE 'queued' END
         WHERE status = 'running'",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

pub async fn run_status(master: &Pool<Sqlite>, run_id: &str) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT status FROM tag_runs WHERE id = ?")
        .bind(run_id)
        .fetch_optional(master)
        .await
}

/// Pauses a `running` run. `reason` is shown in the Runs panel; a pause you asked for has none.
/// Returns whether the run was paused.
///
/// A `queued` run is still being created and can't be paused: resumed before `create_run`
/// finished, it could end `done` with later batches still to come, which would then sit
/// `queued` in a run nobody schedules. It can be cancelled (`finish_creating` skips what comes
/// after), and restart recovery pauses one a crash cut off.
pub async fn pause_run(
    master: &Pool<Sqlite>,
    run_id: &str,
    reason: Option<&str>,
) -> sqlx::Result<bool> {
    let paused = sqlx::query(
        "UPDATE tag_runs SET status = 'paused', pause_reason = ?
         WHERE id = ? AND status = 'running'",
    )
    .bind(reason)
    .bind(run_id)
    .execute(master)
    .await?;
    Ok(paused.rows_affected() > 0)
}

/// Sets the `paused` runs among `run_ids` running again. Returns the ones it resumed.
pub async fn resume_runs(master: &Pool<Sqlite>, run_ids: &[String]) -> sqlx::Result<Vec<String>> {
    let mut resumed = Vec::new();
    for chunk in run_ids.chunks(BATCH) {
        let sql = format!(
            "UPDATE tag_runs SET status = 'running', pause_reason = NULL
             WHERE status = 'paused' AND id IN ({}) RETURNING id",
            vec!["?"; chunk.len()].join(", ")
        );
        let mut query = sqlx::query_scalar::<_, String>(&sql);
        for id in chunk {
            query = query.bind(id);
        }
        resumed.extend(query.fetch_all(master).await?);
    }
    Ok(resumed)
}

/// A paused run, and how it reaches its provider.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct PausedRun {
    pub id: String,
    pub provider: String,
    pub access: Option<String>,
}

/// Every paused run, oldest first.
pub async fn paused_runs(master: &Pool<Sqlite>) -> sqlx::Result<Vec<PausedRun>> {
    sqlx::query_as(
        "SELECT id, provider, access FROM tag_runs WHERE status = 'paused'
         ORDER BY created_at, id",
    )
    .fetch_all(master)
    .await
}

/// Every run that can still send a request: `queued`, `running` or `paused`, or with an item
/// still in flight, as a cancelled run can have.
pub async fn active_run_ids(master: &Pool<Sqlite>) -> sqlx::Result<HashSet<String>> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM tag_runs WHERE status IN ('queued', 'running', 'paused')
         UNION SELECT run_id FROM tag_run_items WHERE status = 'running'",
    )
    .fetch_all(master)
    .await?;
    Ok(ids.into_iter().collect())
}

/// Cancels a `queued`, `running` or `paused` run, in one transaction: its queued items become
/// `skipped`. Items in flight finish, and every tag already applied stays. Returns whether the
/// run was cancelled.
pub async fn cancel_run(master: &Pool<Sqlite>, run_id: &str) -> sqlx::Result<bool> {
    let mut tx = master.begin().await?;
    let cancelled = sqlx::query(
        "UPDATE tag_runs SET status = 'cancelled', pause_reason = NULL, finished_at = ?
         WHERE id = ? AND status IN ('queued', 'running', 'paused')",
    )
    .bind(now_ms())
    .bind(run_id)
    .execute(&mut *tx)
    .await?;
    if cancelled.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE tag_run_items SET status = 'skipped' WHERE run_id = ? AND status = 'queued'",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// Gives back an item that stopped before its result was written: it goes back to the queue,
/// or is `skipped` when its run was cancelled meanwhile.
pub async fn release_item(master: &Pool<Sqlite>, item: &Item) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE tag_run_items
         SET status = CASE WHEN (SELECT status FROM tag_runs WHERE id = ?) = 'cancelled'
                           THEN 'skipped' ELSE 'queued' END
         WHERE run_id = ? AND asset_id = ? AND status = 'running'",
    )
    .bind(&item.run_id)
    .bind(&item.run_id)
    .bind(&item.asset_id)
    .execute(master)
    .await
    .map(|_| ())
}

/// Counts one more call for the item, before a retry.
pub async fn count_attempt(master: &Pool<Sqlite>, item: &Item) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE tag_run_items SET attempts = attempts + 1 WHERE run_id = ? AND asset_id = ?",
    )
    .bind(&item.run_id)
    .bind(&item.asset_id)
    .execute(master)
    .await
    .map(|_| ())
}

/// The run's new tag names still waiting for review, with their suggested parents, oldest first.
pub async fn pending_proposals(
    master: &Pool<Sqlite>,
    run_id: &str,
) -> sqlx::Result<Vec<ProposedTag>> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT name, parent_id FROM tag_run_proposals
         WHERE run_id = ? AND status = 'pending' ORDER BY rowid",
    )
    .bind(run_id)
    .fetch_all(master)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, parent_id)| ProposedTag { name, parent_id })
        .collect())
}

/// The tags this Create freely run created, as later assets see them (D26): their current
/// names and parents, oldest first, with their ids.
pub async fn created_tags(
    master: &Pool<Sqlite>,
    run_id: &str,
) -> sqlx::Result<Vec<(String, ProposedTag)>> {
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT t.id, t.name, t.parent_id
         FROM tag_run_proposals p JOIN tags t ON t.id = p.final_tag_id
         WHERE p.run_id = ? AND p.created_tag = 1 ORDER BY p.rowid",
    )
    .bind(run_id)
    .fetch_all(master)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, parent_id)| (id, ProposedTag { name, parent_id }))
        .collect())
}

/// What an item does with the new tag names in its result.
#[derive(Debug, Clone, Copy)]
pub enum NewNames<'a> {
    /// Prefer existing: merged into the run's proposals, for review.
    Propose(&'a [ProposedTag]),
    /// Create freely: each becomes a tag, recorded as the run's, and is applied.
    Create(&'a [ProposedTag]),
}

/// Write protocol step 1, in one master transaction: the item's new tag names merged into the
/// run's proposals by normalized name, or created as tags (`NewNames`), then a `pending` row
/// for every pair of `tag_ids` and created tags the library lacks (`present` holds the asset's
/// tags), then the item `done` with its result, and the run's usage and notice from the
/// provider's `outcome`. Returns the tags it created.
///
/// The transaction takes the write lock before it reads the catalog, so no other creator (an
/// item, a review, you) can add the same name between the read and the insert, and items
/// proposing a name at once create it once.
pub async fn record_intent(
    master: &Pool<Sqlite>,
    item: &Item,
    tag_ids: &[String],
    present: &HashSet<String>,
    names: NewNames<'_>,
    result: &serde_json::Value,
    outcome: &super::provider::TagResult,
) -> sqlx::Result<Vec<String>> {
    // Taking the write lock up front: a deferred transaction that reads the catalog first
    // would hold a read lock, and SQLite fails it at once rather than wait when another
    // connection is already writing.
    let mut tx = master.begin_with("BEGIN IMMEDIATE").await?;
    let mut tag_ids = tag_ids.to_vec();
    let mut created = Vec::new();
    match names {
        NewNames::Propose(proposals) => propose(&mut tx, item, proposals).await?,
        NewNames::Create(proposals) => {
            for (tag_id, new) in create(&mut tx, item, proposals).await? {
                if new {
                    created.push(tag_id.clone());
                }
                tag_ids.push(tag_id);
            }
        }
    }
    let mut seen = HashSet::new();
    tag_ids.retain(|id| !present.contains(id) && seen.insert(id.clone()));
    let pairs: Vec<PendingPair> = tag_ids
        .iter()
        .map(|tag_id| PendingPair {
            run_id: item.run_id.clone(),
            asset_id: item.asset_id.clone(),
            library_id: item.library_id.clone(),
            tag_id: tag_id.clone(),
            run_cleared: false,
        })
        .collect();
    insert_pending(&mut tx, &pairs).await?;
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
                             cost_usd = cost_usd + ?, notice = ?
         WHERE id = ?",
    )
    .bind(outcome.usage.input_tokens)
    .bind(outcome.usage.output_tokens)
    .bind(outcome.usage.cost_usd)
    .bind(&outcome.notice)
    .bind(&item.run_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(created)
}

/// Merges the item's new tag names into the run's proposals, for review.
async fn propose(
    tx: &mut sqlx::SqliteConnection,
    item: &Item,
    proposals: &[ProposedTag],
) -> sqlx::Result<()> {
    for proposal in proposals {
        // The first spelling stays, and the first parent suggested.
        let proposal_id: String = sqlx::query_scalar(
            "INSERT INTO tag_run_proposals (id, run_id, name, normalized_name, parent_id)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (run_id, normalized_name)
             DO UPDATE SET parent_id = COALESCE(tag_run_proposals.parent_id, excluded.parent_id)
             RETURNING id",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&item.run_id)
        .bind(&proposal.name)
        .bind(normalize_name(&proposal.name))
        .bind(&proposal.parent_id)
        .fetch_one(&mut *tx)
        .await?;
        add_proposal_asset(tx, &proposal_id, &item.asset_id).await?;
    }
    Ok(())
}

async fn add_proposal_asset(
    tx: &mut sqlx::SqliteConnection,
    proposal_id: &str,
    asset_id: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO tag_run_proposal_assets (proposal_id, asset_id) VALUES (?, ?)",
    )
    .bind(proposal_id)
    .bind(asset_id)
    .execute(&mut *tx)
    .await
    .map(|_| ())
}

/// Resolves each of the item's new tag names to a tag, creating the ones the catalog lacks, in
/// the caller's transaction. A created tag goes into master `tags` together with its proposal
/// row (`accepted`, `created_tag = 1`), so the run always owns it. Returns each name's tag, and
/// whether this call created it.
///
/// A name the catalog has by now (another item of the run created it, or you did) is that
/// tag. A name the run created before but which was renamed since is still the run's tag.
async fn create(
    tx: &mut sqlx::SqliteConnection,
    item: &Item,
    proposals: &[ProposedTag],
) -> sqlx::Result<Vec<(String, bool)>> {
    let catalog: Vec<(String, String)> = sqlx::query_as("SELECT id, name FROM tags")
        .fetch_all(&mut *tx)
        .await?;
    let mut ids: HashSet<String> = catalog.iter().map(|(id, _)| id.clone()).collect();
    let mut by_name: std::collections::HashMap<String, String> = catalog
        .into_iter()
        .map(|(id, name)| (normalize_name(&name), id))
        .collect();
    let mut resolved = Vec::new();
    for proposal in proposals {
        let normalized = normalize_name(&proposal.name);
        let earlier: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT id, final_tag_id FROM tag_run_proposals
             WHERE run_id = ? AND normalized_name = ?",
        )
        .bind(&item.run_id)
        .bind(&normalized)
        .fetch_optional(&mut *tx)
        .await?;
        let (mut proposal_id, earlier_tag) = earlier.unzip();
        let known = by_name
            .get(&normalized)
            .cloned()
            .or_else(|| earlier_tag.flatten().filter(|id| ids.contains(id)));
        let (tag_id, new) = match known {
            Some(tag_id) => (tag_id, false),
            None => {
                let tag_id = uuid::Uuid::new_v4().to_string();
                let parent_id = proposal.parent_id.clone().filter(|p| ids.contains(p));
                create_master_tag(tx, &tag_id, &proposal.name, parent_id.as_deref()).await?;
                // A row left from a tag of the run that was deleted since now owns this one.
                proposal_id = Some(
                    sqlx::query_scalar(
                        "INSERT INTO tag_run_proposals
                             (id, run_id, name, normalized_name, parent_id, status, final_tag_id,
                              created_tag)
                         VALUES (?, ?, ?, ?, ?, 'accepted', ?, 1)
                         ON CONFLICT (run_id, normalized_name)
                         DO UPDATE SET name = excluded.name, parent_id = excluded.parent_id,
                                       status = 'accepted', final_tag_id = excluded.final_tag_id,
                                       created_tag = 1
                         RETURNING id",
                    )
                    .bind(uuid::Uuid::new_v4().to_string())
                    .bind(&item.run_id)
                    .bind(&proposal.name)
                    .bind(&normalized)
                    .bind(&parent_id)
                    .bind(&tag_id)
                    .fetch_one(&mut *tx)
                    .await?,
                );
                ids.insert(tag_id.clone());
                by_name.insert(normalized, tag_id.clone());
                (tag_id, true)
            }
        };
        if let Some(proposal_id) = proposal_id {
            add_proposal_asset(tx, &proposal_id, &item.asset_id).await?;
        }
        resolved.push((tag_id, new));
    }
    Ok(resolved)
}

/// Write protocol step 2: a created tag, with its ancestors, into every open library's mirror.
/// Idempotent, and `sync_open_libraries` repairs a mirror that misses it at the next open.
pub async fn mirror_tag<'a>(
    db: &Database,
    libraries: impl IntoIterator<Item = &'a LibraryHandle>,
    tag_id: &str,
) {
    let chain = match db.tag_with_ancestors(tag_id).await {
        Ok(chain) => chain,
        Err(e) => {
            tracing::warn!("Could not read created tag {tag_id} to mirror it: {e}");
            return;
        }
    };
    for lib in libraries {
        let mirror = async {
            let mut tx = lib.pool.begin().await?;
            for definition in &chain {
                upsert_tag(&mut *tx, definition).await?;
            }
            tx.commit().await
        };
        if let Err(e) = mirror.await {
            tracing::warn!("Failed to mirror tag {tag_id} to library {}: {e}", lib.id);
        }
    }
}

/// Write protocol step 1's record of the pairs about to be written, inside the caller's master
/// transaction: a `pending` row for each, which replay applies and undo removes. A pair the run
/// already recorded keeps its row.
pub async fn insert_pending(
    tx: &mut sqlx::SqliteConnection,
    pairs: &[PendingPair],
) -> sqlx::Result<()> {
    for pair in pairs {
        sqlx::query(
            "INSERT OR IGNORE INTO tag_run_applied (run_id, asset_id, library_id, tag_id, state)
             VALUES (?, ?, ?, ?, 'pending')",
        )
        .bind(&pair.run_id)
        .bind(&pair.asset_id)
        .bind(&pair.library_id)
        .bind(&pair.tag_id)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
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

/// Replays every `pending` row whose library is open (steps 3 and 4, both idempotent), or only
/// those of `library` when given. Returns the assets whose tags changed.
///
/// Replaying a library is safe only while no item can be writing to it: an item between its
/// step 1 and step 3 has `pending` rows too, and replay claiming them first makes the item's own
/// step 3 delete them as someone else's pairs, leaving tags undo can't find. So this runs at
/// startup, before any scheduler; a library opening while runs are going instead takes its
/// rows with `pending_rows` before it opens and replays only those with `replay_rows`.
pub async fn replay_pending(db: &Database, library: Option<&str>) -> sqlx::Result<Vec<String>> {
    let rows = pending_rows(db, library).await?;
    Ok(replay_rows(db, rows).await)
}

/// Every `pending` row, or those of `library` when given, except rows of runs being undone:
/// they wait to be removed, not applied.
pub async fn pending_rows(db: &Database, library: Option<&str>) -> sqlx::Result<Vec<PendingPair>> {
    sqlx::query_as(
        "SELECT a.run_id, a.asset_id, a.library_id, a.tag_id, (r.id IS NULL) AS run_cleared
         FROM tag_run_applied a LEFT JOIN tag_runs r ON r.id = a.run_id
         WHERE a.state = 'pending' AND (?1 IS NULL OR a.library_id = ?1)
           AND (r.status IS NULL OR r.status NOT IN ('undoing', 'undo_partial', 'undone'))
         ORDER BY a.library_id, a.asset_id, a.tag_id",
    )
    .bind(library)
    .fetch_all(&db.master_pool)
    .await
}

/// Write protocol steps 3 and 4 for `rows`, as replay. Rows for an offline library wait for
/// it, and one library's failure (a NAS lock timeout) leaves its rows for the next replay.
/// Returns the assets whose tags changed.
pub async fn replay_rows(db: &Database, rows: Vec<PendingPair>) -> Vec<String> {
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
        match apply_pairs(db, &lib, &pairs, Applier::Replay).await {
            Ok(assets) => changed.extend(assets),
            Err(e) => tracing::warn!("Pending tags for library {library_id} wait: {e}"),
        }
    }
    changed
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

        let changed = replay_pending(&db, None).await.unwrap();

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

    #[tokio::test]
    async fn restart_skips_a_cancelled_runs_in_flight_items_and_flags_half_made_runs() {
        let db = memory_database(Vec::new()).await;
        let master = &db.master_pool;
        insert_run(&db, "cancelled-run", "cancelled").await;
        insert_run(&db, "running-run", "running").await;
        insert_run(&db, "half-made-run", "queued").await;
        for run in ["cancelled-run", "half-made-run", "running-run"] {
            sqlx::query(
                "INSERT INTO tag_run_items (run_id, asset_id, library_id, status)
                 VALUES (?, 'a1', 'lib', 'running')",
            )
            .bind(run)
            .execute(master)
            .await
            .unwrap();
        }

        recover_after_restart(master).await.unwrap();

        let rows: Vec<(String, String, Option<String>, String)> = sqlx::query_as(
            "SELECT r.id, r.status, r.pause_reason, i.status
             FROM tag_runs r JOIN tag_run_items i ON i.run_id = r.id ORDER BY r.id",
        )
        .fetch_all(master)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [
                (
                    "cancelled-run".into(),
                    "cancelled".into(),
                    None,
                    "skipped".into()
                ),
                (
                    "half-made-run".into(),
                    "paused".into(),
                    Some(
                        "app restarted while the run was starting: it holds only 1 asset, \
                         and any others were never added"
                            .into()
                    ),
                    "queued".into()
                ),
                (
                    "running-run".into(),
                    "paused".into(),
                    Some(APP_RESTARTED.into()),
                    "queued".into()
                ),
            ]
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
            assert!(replay_pending(&db, None).await.unwrap().len() <= 1);
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
    async fn replaying_one_library_leaves_another_librarys_in_flight_item_its_rows() {
        let a = memory_library_pool().await;
        let b = memory_library_pool().await;
        insert_asset(&a, "a1").await;
        insert_asset(&b, "b1").await;
        let db = memory_database(vec![
            memory_library("a-lib", a.clone()),
            memory_library("b-lib", b.clone()),
        ])
        .await;
        insert_tag(&db.master_pool, "t1", "Rock").await;
        insert_run(&db, "run", "running").await;
        // a1's item has recorded its intent (step 1) and is about to write library A.
        insert_pending(&db, "run", "a1", "a-lib", "t1").await;
        // b1's write waited for library B, which just opened.
        insert_pending(&db, "run", "b1", "b-lib", "t1").await;

        let replayed = replay_pending(&db, Some("b-lib")).await.unwrap();

        assert_eq!(replayed, ["b1"]);
        assert!(
            asset_tags(&a).await.is_empty(),
            "library A is left to its item"
        );
        let lib = db.get_library_handle("a-lib").await.unwrap();
        let in_flight: Vec<PendingPair> = sqlx::query_as(
            "SELECT run_id, asset_id, library_id, tag_id, 0 AS run_cleared FROM tag_run_applied
             WHERE asset_id = 'a1'",
        )
        .fetch_all(&db.master_pool)
        .await
        .unwrap();
        let written = apply_pairs(&db, &lib, &in_flight, Applier::Item)
            .await
            .unwrap();

        assert_eq!(written, ["a1"]);
        assert_eq!(
            applied_rows(&db).await,
            [
                ("a1".into(), "t1".into(), "applied".into()),
                ("b1".into(), "t1".into(), "applied".into()),
            ],
            "the item keeps its row, so undo can still remove the tag"
        );
    }

    async fn insert_item(db: &Database, run: &str, asset: &str, status: &str) {
        sqlx::query(
            "INSERT INTO tag_run_items (run_id, asset_id, library_id, status)
             VALUES (?, ?, 'lib', ?)",
        )
        .bind(run)
        .bind(asset)
        .bind(status)
        .execute(&db.master_pool)
        .await
        .unwrap();
    }

    async fn items_of(db: &Database, run: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT status FROM tag_run_items WHERE run_id = ? ORDER BY asset_id")
            .bind(run)
            .fetch_all(&db.master_pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_cancel_during_creation_skips_the_items_inserted_after_it() {
        let db = memory_database(Vec::new()).await;
        // Cancelled after its first batch went in: that batch is skipped, later ones queued.
        insert_run(&db, "cancelled", "cancelled").await;
        insert_item(&db, "cancelled", "a1", "skipped").await;
        insert_item(&db, "cancelled", "a2", "queued").await;
        insert_item(&db, "cancelled", "a3", "queued").await;
        insert_run(&db, "paused", "paused").await;
        insert_item(&db, "paused", "a1", "queued").await;
        insert_run(&db, "fresh", "queued").await;
        insert_item(&db, "fresh", "a1", "queued").await;

        for run in ["cancelled", "paused", "fresh"] {
            finish_creating(&db.master_pool, run).await.unwrap();
        }

        assert_eq!(items_of(&db, "cancelled").await, ["skipped"; 3]);
        assert_eq!(
            items_of(&db, "paused").await,
            ["queued"],
            "resume hands them out"
        );
        assert_eq!(items_of(&db, "fresh").await, ["queued"]);
        let statuses: Vec<(String, String)> =
            sqlx::query_as("SELECT id, status FROM tag_runs ORDER BY id")
                .fetch_all(&db.master_pool)
                .await
                .unwrap();
        assert_eq!(
            statuses,
            [
                ("cancelled".into(), "cancelled".into()),
                ("fresh".into(), "running".into()),
                ("paused".into(), "paused".into()),
            ]
        );
    }

    #[tokio::test]
    async fn replaying_the_rows_read_before_a_library_opened_leaves_a_new_items_rows() {
        let lib_pool = memory_library_pool().await;
        insert_asset(&lib_pool, "a1").await;
        insert_asset(&lib_pool, "a2").await;
        let db = memory_database(vec![memory_library("lib", lib_pool.clone())]).await;
        insert_tag(&db.master_pool, "t1", "Rock").await;
        insert_run(&db, "old-run", "done").await;
        insert_run(&db, "running-run", "running").await;
        // Stranded while the library was offline.
        insert_pending(&db, "old-run", "a1", "lib", "t1").await;
        let waiting = pending_rows(&db, Some("lib")).await.unwrap();
        // The library opens, and an item claims a2 and records its intent before replay runs.
        insert_pending(&db, "running-run", "a2", "lib", "t1").await;

        let replayed = replay_rows(&db, waiting).await;

        assert_eq!(replayed, ["a1"]);
        assert_eq!(asset_tags(&lib_pool).await, [("a1".into(), "t1".into())]);
        assert_eq!(
            applied_rows(&db).await,
            [
                ("a1".into(), "t1".into(), "applied".into()),
                ("a2".into(), "t1".into(), "pending".into()),
            ],
            "the item's row is left to its own step 3"
        );
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

        let changed = replay_pending(&db, None).await.unwrap();

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

    #[tokio::test]
    async fn a_create_freely_name_keeps_the_run_tag_it_was_renamed_to_and_recreates_a_deleted_one()
    {
        let db = memory_database(vec![]).await;
        let master = &db.master_pool;
        insert_run(&db, "r", "running").await;
        /// One Create freely item over `asset` that proposes "Moss". Returns the tags it created.
        async fn intent(master: &Pool<Sqlite>, asset: &str) -> Vec<String> {
            sqlx::query(
                "INSERT INTO tag_run_items (run_id, asset_id, library_id) VALUES ('r', ?, 'lib')",
            )
            .bind(asset)
            .execute(master)
            .await
            .unwrap();
            let item = Item {
                run_id: "r".into(),
                asset_id: asset.into(),
                library_id: "lib".into(),
                mode: "create_freely".into(),
                access: None,
                model: None,
                options: serde_json::json!({}),
            };
            let moss = [ProposedTag {
                name: "Moss".into(),
                parent_id: None,
            }];
            record_intent(
                master,
                &item,
                &[],
                &HashSet::new(),
                NewNames::Create(&moss),
                &serde_json::json!({}),
                &Default::default(),
            )
            .await
            .unwrap()
        }
        let owned = || async {
            sqlx::query_as::<_, (String, String)>(
                "SELECT p.final_tag_id, group_concat(pa.asset_id, ',')
                 FROM tag_run_proposals p JOIN tag_run_proposal_assets pa ON pa.proposal_id = p.id
                 WHERE p.created_tag = 1 GROUP BY p.id",
            )
            .fetch_all(master)
            .await
            .unwrap()
        };

        let created = intent(master, "a1").await;
        assert_eq!(created.len(), 1);
        sqlx::query("UPDATE tags SET name = 'Lichen'")
            .execute(master)
            .await
            .unwrap();
        assert!(
            intent(master, "a2").await.is_empty(),
            "renamed, it is still the run's Moss"
        );
        assert_eq!(owned().await, [(created[0].clone(), "a1,a2".into())]);

        sqlx::query("DELETE FROM tags")
            .execute(master)
            .await
            .unwrap();
        let recreated = intent(master, "a3").await;
        assert_eq!(recreated.len(), 1);
        assert_ne!(recreated, created);
        assert_eq!(
            owned().await,
            [(recreated[0].clone(), "a1,a2,a3".into())],
            "the run owns the new tag"
        );
        let pairs: Vec<(String, String)> =
            sqlx::query_as("SELECT asset_id, tag_id FROM tag_run_applied ORDER BY asset_id")
                .fetch_all(master)
                .await
                .unwrap();
        assert_eq!(
            pairs,
            [
                ("a1".into(), created[0].clone()),
                ("a2".into(), created[0].clone()),
                ("a3".into(), recreated[0].clone()),
            ]
        );
    }
}
