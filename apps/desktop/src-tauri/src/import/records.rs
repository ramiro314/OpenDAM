//! The import records in the library DB (plan pack-import, Records): imports, their packs and
//! steps, each build's output manifest, the journal of rows a build inserted, and the runs.

use std::sync::atomic::{AtomicI64, Ordering};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{Pool, Sqlite, SqliteConnection};

use super::place::PlaceStep;
use super::unitypackage::SkippedEntry;
use crate::watcher::InsertJournal;

/// A file a build staged, as its manifest row holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Relative to the library root.
    pub path: String,
    pub size: u64,
    pub mtime_ns: i64,
    pub sha256: String,
    /// Empty for the metadata step's `datapackage.json`, which comes from no entry.
    pub source_path: Vec<String>,
    /// Made by a step (`datapackage.json`), not extracted.
    pub generated: bool,
}

/// One pack of a run, with the `recipe_rev` it was queued at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedPack {
    pub pack_id: String,
    pub recipe_rev: i64,
}

/// A run as `import:updated` and the run list show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub id: String,
    pub library_id: String,
    pub kind: String,
    pub packs: Vec<QueuedPack>,
    pub status: String,
    pub phase: Option<String>,
    pub progress_done: i64,
    pub progress_total: i64,
    pub error: Option<String>,
    pub warnings: Vec<SkippedEntry>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

type RunRow = (
    String,
    String,
    String,
    String,
    Option<String>,
    i64,
    i64,
    Option<String>,
    String,
    i64,
    Option<i64>,
    Option<i64>,
);

const RUN_COLUMNS: &str = "id, kind, packs, status, phase, progress_done, progress_total, error,
    warnings, created_at, started_at, finished_at";

fn summary(library_id: &str, row: RunRow) -> RunSummary {
    let (id, kind, packs, status, phase, done, total, error, warnings, created, started, finished) =
        row;
    RunSummary {
        id,
        library_id: library_id.to_string(),
        kind,
        packs: serde_json::from_str(&packs).unwrap_or_default(),
        status,
        phase,
        progress_done: done,
        progress_total: total,
        error,
        warnings: serde_json::from_str(&warnings).unwrap_or_default(),
        created_at: created,
        started_at: started,
        finished_at: finished,
    }
}

pub async fn run(pool: &Pool<Sqlite>, library_id: &str, id: &str) -> sqlx::Result<RunSummary> {
    let row: RunRow = sqlx::query_as(&format!(
        "SELECT {RUN_COLUMNS} FROM import_runs WHERE id = ?"
    ))
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(summary(library_id, row))
}

/// The library's runs, newest first.
pub async fn runs(pool: &Pool<Sqlite>, library_id: &str) -> sqlx::Result<Vec<RunSummary>> {
    let rows: Vec<RunRow> = sqlx::query_as(&format!(
        "SELECT {RUN_COLUMNS} FROM import_runs ORDER BY created_at DESC, rowid DESC"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| summary(library_id, r)).collect())
}

/// A run's owner: the process that queued it, then the one that runs it.
pub type Owner = crate::watcher::ProcessIdentity;

/// A step as the caller gives it, before it is stored.
#[derive(Debug, Clone)]
pub struct StepRecord {
    pub kind: String,
    pub options: Value,
}

impl StepRecord {
    pub fn place(step: &PlaceStep) -> Self {
        let mut options = serde_json::to_value(step).expect("a place step serializes");
        let kind = options
            .as_object_mut()
            .and_then(|o| o.remove("kind"))
            .and_then(|k| k.as_str().map(str::to_string))
            .expect("a place step has a kind");
        StepRecord { kind, options }
    }
}

/// What a new import records: the original, its one pack and that pack's steps.
pub struct NewImport<'a> {
    pub original_path: &'a str,
    pub original_size: i64,
    pub original_mtime_ns: i64,
    pub original_sha256: &'a str,
    pub kind: &'a str,
    /// The pack's source in the original: its variant and sub-folder, `/`-joined, or `""`.
    pub source: &'a str,
    pub target: &'a str,
    pub preset: &'a str,
    pub steps: &'a [StepRecord],
}

/// Another pack's target that is `?1`, or an ancestor or descendant of it, on whole path
/// components (`Pub/Pack` holds `Pub/Pack/Sub` but not `Pub/Package`).
const NESTING: &str = "SELECT target FROM import_packs WHERE target = ?1
    OR substr(?1, 1, length(target) + 1) = target || '/'
    OR substr(target, 1, length(?1) + 1) = ?1 || '/'";

/// Records `import` and queues its first build, owned by `owner`, in one transaction. Returns
/// the run's id, or as the inner error another pack's target that nests with this one: the
/// pack's insert checks that in the same statement, so imports that overlap, from this process
/// or another, never both record nesting targets.
pub async fn create_import(
    pool: &Pool<Sqlite>,
    import: &NewImport<'_>,
    owner: &Owner,
    now: i64,
) -> sqlx::Result<Result<String, String>> {
    let import_id = uuid::Uuid::new_v4().to_string();
    let pack_id = uuid::Uuid::new_v4().to_string();
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO imports (id, original_path, original_size, original_mtime_ns,
         original_sha256, kind, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&import_id)
    .bind(import.original_path)
    .bind(import.original_size)
    .bind(import.original_mtime_ns)
    .bind(import.original_sha256)
    .bind(import.kind)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let inserted = sqlx::query(&format!(
        "INSERT INTO import_packs (id, import_id, target, preset, source)
         SELECT ?2, ?3, ?1, ?4, ?5 WHERE NOT EXISTS ({NESTING})"
    ))
    .bind(import.target)
    .bind(&pack_id)
    .bind(&import_id)
    .bind(import.preset)
    .bind(import.source)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        let other: String = sqlx::query_scalar(NESTING)
            .bind(import.target)
            .fetch_one(&mut *tx)
            .await?;
        return Ok(Err(other));
    }
    for (position, step) in import.steps.iter().enumerate() {
        sqlx::query(
            "INSERT INTO import_steps (pack_id, position, kind, enabled, options)
             VALUES (?, ?, ?, 1, ?)",
        )
        .bind(&pack_id)
        .bind(position as i64)
        .bind(&step.kind)
        .bind(step.options.to_string())
        .execute(&mut *tx)
        .await?;
    }
    let packs = json!([QueuedPack {
        pack_id,
        recipe_rev: 0
    }]);
    sqlx::query(
        "INSERT INTO import_runs (id, kind, packs, status, created_at, owner_machine, owner_boot,
         owner_pid, owner_started, heartbeat_at) VALUES (?, 'import', ?, 'queued', ?, ?, ?, ?, ?, ?)",
    )
    .bind(&run_id)
    .bind(packs.to_string())
    .bind(now)
    .bind(&owner.machine)
    .bind(&owner.boot)
    .bind(owner.pid as i64)
    .bind(owner.started as i64)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Ok(run_id))
}

/// A pack's recipe as a build reads it.
#[derive(Debug, Clone)]
pub struct Recipe {
    pub pack_id: String,
    pub target: String,
    pub recipe_rev: i64,
    pub current_build: Option<String>,
    pub swap_state: Option<String>,
    pub original_path: String,
    pub original_size: i64,
    pub original_mtime_ns: i64,
    pub kind: String,
    /// The Kenney store page lookup's answer, `metadata::StorePage` as JSON, `None` before one.
    pub store_page: Option<String>,
    /// Enabled steps in position order, as `(kind, options)`.
    pub steps: Vec<(String, Value)>,
}

pub async fn recipe(pool: &Pool<Sqlite>, pack_id: &str) -> sqlx::Result<Recipe> {
    #[allow(clippy::type_complexity)]
    let (target, recipe_rev, current_build, swap_state, original_path, size, mtime, kind, page): (
        String,
        i64,
        Option<String>,
        Option<String>,
        String,
        i64,
        i64,
        String,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT p.target, p.recipe_rev, p.current_build, p.swap_state, i.original_path,
         i.original_size, i.original_mtime_ns, i.kind, p.store_page
         FROM import_packs p JOIN imports i ON i.id = p.import_id WHERE p.id = ?",
    )
    .bind(pack_id)
    .fetch_one(pool)
    .await?;
    let steps: Vec<(String, String)> = sqlx::query_as(
        "SELECT kind, options FROM import_steps WHERE pack_id = ? AND enabled = 1
         ORDER BY position",
    )
    .bind(pack_id)
    .fetch_all(pool)
    .await?;
    Ok(Recipe {
        pack_id: pack_id.to_string(),
        target,
        recipe_rev,
        current_build,
        swap_state,
        original_path,
        original_size: size,
        original_mtime_ns: mtime,
        kind,
        store_page: page,
        steps: steps
            .into_iter()
            .map(|(k, o)| (k, serde_json::from_str(&o).unwrap_or(Value::Null)))
            .collect(),
    })
}

/// A manifest row as a rebuild reads it: the previous build's files, to match its own against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestRow {
    pub path: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub sha256: String,
    /// The JSON array `import_outputs` stores, `NULL` for a kept file added outside OpenDAM.
    pub source_path: Option<String>,
    pub generated: bool,
}

/// Build `build_id`'s manifest rows, sorted by path.
pub async fn manifest(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    build_id: &str,
) -> sqlx::Result<Vec<ManifestRow>> {
    let rows: Vec<(String, i64, i64, String, Option<String>, bool)> = sqlx::query_as(
        "SELECT path, size, mtime_ns, sha256, source_path, origin = 'generated'
         FROM import_outputs WHERE pack_id = ? AND build_id = ? ORDER BY path",
    )
    .bind(pack_id)
    .bind(build_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(path, size, mtime_ns, sha256, source_path, generated)| ManifestRow {
                path,
                size,
                mtime_ns,
                sha256,
                source_path,
                generated,
            },
        )
        .collect())
}

/// A source path as `import_outputs` stores it, a JSON array.
pub fn source_json(source_path: &[String]) -> String {
    serde_json::to_string(source_path).expect("strings serialize")
}

/// One of a pack's steps, as the step editor reads and writes them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub kind: String,
    pub enabled: bool,
    #[serde(default = "empty_options")]
    pub options: Value,
}

fn empty_options() -> Value {
    json!({})
}

/// A pack's steps in position order, with the `recipe_rev` they are at.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackSteps {
    pub recipe_rev: i64,
    pub steps: Vec<Step>,
}

pub async fn steps(pool: &Pool<Sqlite>, pack_id: &str) -> sqlx::Result<Option<PackSteps>> {
    let Some(recipe_rev) = sqlx::query_scalar("SELECT recipe_rev FROM import_packs WHERE id = ?")
        .bind(pack_id)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    let rows: Vec<(String, bool, String)> = sqlx::query_as(
        "SELECT kind, enabled, options FROM import_steps WHERE pack_id = ? ORDER BY position",
    )
    .bind(pack_id)
    .fetch_all(pool)
    .await?;
    let steps = rows
        .into_iter()
        .map(|(kind, enabled, options)| Step {
            kind,
            enabled,
            options: serde_json::from_str(&options).unwrap_or(Value::Null),
        })
        .collect();
    Ok(Some(PackSteps { recipe_rev, steps }))
}

/// Why a pack's steps can't be edited, or it can't be rebuilt, now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    NoPack,
    /// A run for the pack is queued or running, or a swap or Remove is journaled.
    Busy,
}

/// The `import_packs` row is busy: a run for it is queued or running, or its `swap_state` is set.
const BUSY: &str = "(import_packs.swap_state IS NOT NULL OR EXISTS (
    SELECT 1 FROM import_runs r, json_each(r.packs) j
    WHERE r.status IN ('queued', 'running')
    AND json_extract(j.value, '$.packId') = import_packs.id))";

async fn refusal(tx: &mut SqliteConnection, pack_id: &str) -> sqlx::Result<Refused> {
    let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM import_packs WHERE id = ?")
        .bind(pack_id)
        .fetch_optional(&mut *tx)
        .await?;
    Ok(if exists.is_some() {
        Refused::Busy
    } else {
        Refused::NoPack
    })
}

/// Replaces pack `pack_id`'s steps with `steps` and bumps its `recipe_rev`, in one transaction
/// whose first write checks that no run for the pack is queued or running. Returns the new
/// `recipe_rev`.
pub async fn set_steps(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    steps: &[Step],
) -> sqlx::Result<Result<i64, Refused>> {
    let mut tx = pool.begin().await?;
    let bumped: Option<i64> = sqlx::query_scalar(&format!(
        "UPDATE import_packs SET recipe_rev = recipe_rev + 1 WHERE id = ? AND NOT {BUSY}
         RETURNING recipe_rev"
    ))
    .bind(pack_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(recipe_rev) = bumped else {
        return Ok(Err(refusal(&mut tx, pack_id).await?));
    };
    sqlx::query("DELETE FROM import_steps WHERE pack_id = ?")
        .bind(pack_id)
        .execute(&mut *tx)
        .await?;
    for (position, step) in steps.iter().enumerate() {
        sqlx::query(
            "INSERT INTO import_steps (pack_id, position, kind, enabled, options)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(pack_id)
        .bind(position as i64)
        .bind(&step.kind)
        .bind(step.enabled)
        .bind(step.options.to_string())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Ok(recipe_rev))
}

/// Queues a rebuild of pack `pack_id` at its current `recipe_rev`, owned by `owner`, unless a
/// run for it is queued or running: one statement checks and inserts. Returns the run's id.
pub async fn queue_rebuild(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    owner: &Owner,
    now: i64,
) -> sqlx::Result<Result<String, Refused>> {
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut tx = pool.begin().await?;
    let queued = sqlx::query(&format!(
        "INSERT INTO import_runs (id, kind, packs, status, created_at, owner_machine, owner_boot,
         owner_pid, owner_started, heartbeat_at)
         SELECT ?, 'rebuild', json_array(json_object('packId', id, 'recipeRev', recipe_rev)),
         'queued', ?, ?, ?, ?, ?, ? FROM import_packs WHERE id = ? AND NOT {BUSY}"
    ))
    .bind(&run_id)
    .bind(now)
    .bind(&owner.machine)
    .bind(&owner.boot)
    .bind(owner.pid as i64)
    .bind(owner.started as i64)
    .bind(now)
    .bind(pack_id)
    .execute(&mut *tx)
    .await?;
    if queued.rows_affected() == 0 {
        return Ok(Err(refusal(&mut tx, pack_id).await?));
    }
    tx.commit().await?;
    Ok(Ok(run_id))
}

/// Writes `outputs` as build `build_id`'s manifest rows, in 200-row transactions.
/// They mean nothing until the build commits.
pub async fn write_outputs(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    build_id: &str,
    outputs: &[Output],
) -> sqlx::Result<()> {
    for chunk in outputs.chunks(200) {
        let mut tx = pool.begin().await?;
        for output in chunk {
            sqlx::query(
                "INSERT INTO import_outputs (pack_id, build_id, path, size, mtime_ns, sha256,
                 origin, source_path) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(pack_id)
            .bind(build_id)
            .bind(&output.path)
            .bind(output.size as i64)
            .bind(output.mtime_ns)
            .bind(&output.sha256)
            .bind(if output.generated {
                "generated"
            } else {
                "extracted"
            })
            .bind(source_json(&output.source_path))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

/// Keeps the store page lookup's answer for pack `pack_id` (`metadata::StorePage` as JSON),
/// replacing one for other URLs. It is no change to the pack's steps, so it bumps no
/// `recipe_rev` (plan R24).
pub async fn set_store_page(pool: &Pool<Sqlite>, pack_id: &str, answer: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE import_packs SET store_page = ? WHERE id = ?")
        .bind(answer)
        .bind(pack_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Forgets pack `pack_id`'s store page answer, so its next build looks the page up again.
/// Refused while a run for the pack is queued or running, as a step edit is.
pub async fn clear_store_page(
    pool: &Pool<Sqlite>,
    pack_id: &str,
) -> sqlx::Result<Result<(), Refused>> {
    let mut tx = pool.begin().await?;
    let cleared = sqlx::query(&format!(
        "UPDATE import_packs SET store_page = NULL WHERE id = ? AND NOT {BUSY}"
    ))
    .bind(pack_id)
    .execute(&mut *tx)
    .await?;
    if cleared.rows_affected() == 0 {
        return Ok(Err(refusal(&mut tx, pack_id).await?));
    }
    tx.commit().await?;
    Ok(Ok(()))
}

/// Build `build_id`'s manifest rows as `(path, size)`, sorted by path.
pub async fn output_sizes(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    build_id: &str,
) -> sqlx::Result<Vec<(String, i64)>> {
    sqlx::query_as(
        "SELECT path, size FROM import_outputs WHERE pack_id = ? AND build_id = ? ORDER BY path",
    )
    .bind(pack_id)
    .bind(build_id)
    .fetch_all(pool)
    .await
}

/// Deletes build `build_id`'s manifest rows and journal.
pub async fn delete_build(
    tx: &mut SqliteConnection,
    pack_id: &str,
    build_id: &str,
) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM import_outputs WHERE pack_id = ? AND build_id = ?")
        .bind(pack_id)
        .bind(build_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM import_moves WHERE pack_id = ? AND build_id = ?")
        .bind(pack_id)
        .bind(build_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Journals each row an index run inserts as `inserted` in `import_moves`, in the transaction
/// that inserts it (`watcher::Prune::Defer`).
pub struct InsertedRows {
    pack_id: String,
    build_id: String,
    seq: AtomicI64,
}

impl InsertedRows {
    /// A journal continuing after build `build_id`'s last entry.
    pub async fn after(pool: &Pool<Sqlite>, pack_id: &str, build_id: &str) -> sqlx::Result<Self> {
        let last: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) FROM import_moves WHERE pack_id = ? AND build_id = ?",
        )
        .bind(pack_id)
        .bind(build_id)
        .fetch_one(pool)
        .await?;
        Ok(InsertedRows {
            pack_id: pack_id.to_string(),
            build_id: build_id.to_string(),
            seq: AtomicI64::new(last),
        })
    }
}

impl InsertJournal for InsertedRows {
    fn record<'a>(
        &'a self,
        tx: &'a mut SqliteConnection,
        inserted: &'a [(&'a str, &'a str)],
    ) -> BoxFuture<'a, sqlx::Result<()>> {
        Box::pin(async move {
            for (asset_id, path) in inserted {
                sqlx::query(
                    "INSERT INTO import_moves (pack_id, build_id, seq, kind, asset_id, to_path)
                     VALUES (?, ?, ?, 'inserted', ?, ?)",
                )
                .bind(&self.pack_id)
                .bind(&self.build_id)
                .bind(self.seq.fetch_add(1, Ordering::SeqCst) + 1)
                .bind(asset_id)
                .bind(path)
                .execute(&mut *tx)
                .await?;
            }
            Ok(())
        })
    }
}
