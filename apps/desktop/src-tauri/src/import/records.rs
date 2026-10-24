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
    /// For a generated GLB, its source model's; empty for the metadata step's
    /// `datapackage.json`, which comes from no entry, and for a kept file added outside
    /// OpenDAM, which is stored as `NULL`.
    pub source_path: Vec<String>,
    pub origin: Origin,
}

impl Output {
    /// The source path as `import_outputs` stores it: `None` exactly for a kept file of its own
    /// (added outside OpenDAM), which has no source path. A generated `datapackage.json` has
    /// the empty array, from no entry. This is the one place the rule lives.
    pub fn stored_source(&self) -> Option<String> {
        (self.origin != Origin::Kept || !self.source_path.is_empty())
            .then(|| source_json(&self.source_path))
    }

    /// The file key a rebuild matches the previous build's files by (`carry`, the unchanged
    /// mtime): its stored source path and whether it is generated, `None` for a file without
    /// a source path.
    pub fn file_key(&self) -> Option<(String, bool)> {
        Some((self.stored_source()?, self.origin.generated()))
    }
}

/// The source path (a JSON array, as stored) of the asset whose `.meta` has the source path
/// `source`: its last element without `.meta`, or `None` for a file that isn't a `.meta`. A
/// `.meta`'s own source path is its asset's with `.meta` appended (`place::meta_source`).
pub fn asset_of_meta(source: &str) -> Option<String> {
    let mut source: Vec<String> = serde_json::from_str(source).ok()?;
    let last = source.last_mut()?;
    *last = last.strip_suffix(".meta")?.to_string();
    Some(source_json(&source))
}

/// The source paths (JSON arrays, as stored) the enabled `exclude` steps among `steps` (as a
/// `Recipe` holds them, `(kind, options)`) leave out.
pub fn excluded_sources(steps: &[(String, Value)]) -> std::collections::HashSet<String> {
    steps
        .iter()
        .filter(|(kind, _)| kind == "exclude")
        .filter_map(|(_, options)| {
            let source: Vec<String> = serde_json::from_value(options["source"].clone()).ok()?;
            Some(source_json(&source))
        })
        .collect()
}

/// Whether `exclude` steps of the source paths `excluded` drop the file with the source path
/// `source` (all JSON arrays, as stored): one of its own, or for a `.meta`, its asset's, which
/// drops the `.meta` too.
pub fn leaves_out(excluded: &std::collections::HashSet<String>, source: &str) -> bool {
    excluded.contains(source) || asset_of_meta(source).is_some_and(|a| excluded.contains(&a))
}

/// Where a manifest's file comes from (`import_outputs.origin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// From an entry of the original.
    Extracted,
    /// Made by a step: a `unity_glb` GLB, or the metadata step's `datapackage.json`.
    Generated,
    /// Kept from outside changes ("keep yours" or "keep" at drift): your bytes, carried by every
    /// rebuild, in place of the recipe's file with its source path, or as a file of its own.
    Kept,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Extracted => "extracted",
            Origin::Generated => "generated",
            Origin::Kept => "kept",
        }
    }

    fn parse(origin: &str) -> Self {
        match origin {
            "generated" => Origin::Generated,
            "kept" => Origin::Kept,
            _ => Origin::Extracted,
        }
    }

    /// Whether a file of this origin is generated, the second half of its file key.
    pub fn generated(self) -> bool {
        self == Origin::Generated
    }
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

/// An import as the Imports view lists it: its original and its packs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportSummary {
    pub id: String,
    pub library_id: String,
    /// Absolute.
    pub original_path: String,
    pub kind: String,
    pub created_at: i64,
    pub packs: Vec<PackSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackSummary {
    pub id: String,
    /// Relative to the library root.
    pub target: String,
    pub preset: String,
    /// `built`, `failed` or `never_built`.
    pub status: String,
    pub built_at: Option<i64>,
}

/// The library's imports, newest first, each with its packs by target.
pub async fn imports(pool: &Pool<Sqlite>, library_id: &str) -> sqlx::Result<Vec<ImportSummary>> {
    let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT id, original_path, kind, created_at FROM imports
         ORDER BY created_at DESC, rowid DESC",
    )
    .fetch_all(pool)
    .await?;
    let packs: Vec<(String, String, String, String, String, Option<i64>)> = sqlx::query_as(
        "SELECT import_id, id, target, preset, status, built_at FROM import_packs ORDER BY target",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, original_path, kind, created_at)| ImportSummary {
            packs: packs
                .iter()
                .filter(|p| p.0 == id)
                .map(|(_, id, target, preset, status, built_at)| PackSummary {
                    id: id.clone(),
                    target: target.clone(),
                    preset: preset.clone(),
                    status: status.clone(),
                    built_at: *built_at,
                })
                .collect(),
            id,
            library_id: library_id.to_string(),
            original_path,
            kind,
            created_at,
        })
        .collect())
}

/// A run's owner: the process that queued it, then the one that runs it.
pub type Owner = crate::watcher::ProcessIdentity;

/// A step as the caller gives it, before it is stored.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StepRecord {
    pub kind: String,
    pub enabled: bool,
    pub options: Value,
}

impl StepRecord {
    /// An enabled step.
    pub fn new(kind: &str, options: Value) -> Self {
        StepRecord {
            kind: kind.into(),
            enabled: true,
            options,
        }
    }

    pub fn place(step: &PlaceStep) -> Self {
        let mut options = serde_json::to_value(step).expect("a place step serializes");
        let kind = options
            .as_object_mut()
            .and_then(|o| o.remove("kind"))
            .and_then(|k| k.as_str().map(str::to_string))
            .expect("a place step has a kind");
        StepRecord::new(&kind, options)
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
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&pack_id)
        .bind(position as i64)
        .bind(&step.kind)
        .bind(step.enabled)
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

/// The publisher of the library's latest import, or `""` before its first. A Kenney import's
/// publisher is the preset's own, never one you chose, so it is passed over.
pub async fn last_publisher(pool: &Pool<Sqlite>) -> sqlx::Result<String> {
    let publisher: Option<Option<String>> = sqlx::query_scalar(
        "SELECT json_extract(s.options, '$.publisher') FROM import_steps s
         JOIN import_packs p ON p.id = s.pack_id JOIN imports i ON i.id = p.import_id
         WHERE s.kind = 'metadata' AND json_extract(s.options, '$.publisher') IS NOT NULL
         AND json_extract(s.options, '$.preset') IS NOT 'kenney'
         ORDER BY i.created_at DESC, i.rowid DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(publisher.flatten().unwrap_or_default())
}

/// Why `set_target` left a pack's target as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetRefused {
    /// The library has no such pack.
    NoPack,
    /// Another pack's target nests with the new one.
    Nests(String),
    /// A run for the pack is queued or running.
    Running,
    /// The pack has a committed build, or a swap or Remove under way.
    Built,
}

/// Another pack than `?2` whose target is `?1`, or an ancestor or descendant of it.
const NESTING_OTHER: &str = "SELECT target FROM import_packs WHERE id <> ?2 AND (target = ?1
    OR substr(?1, 1, length(target) + 1) = target || '/'
    OR substr(target, 1, length(?1) + 1) = ?1 || '/')";

/// A queued or running run of pack `?2`.
const LIVE_RUN: &str = "SELECT 1 FROM import_runs r, json_each(r.packs) j
    WHERE r.status IN ('queued', 'running') AND json_extract(j.value, '$.packId') = ?2";

/// Moves pack `pack_id`'s target to `target`, in one statement that checks it nests with no
/// other pack's, no run for the pack is queued or running, and the pack has never been built:
/// moving a built pack's folder is a rebuild's work.
pub async fn set_target(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    target: &str,
) -> sqlx::Result<Result<(), TargetRefused>> {
    let moved = sqlx::query(&format!(
        "UPDATE import_packs SET target = ?1 WHERE id = ?2 AND current_build IS NULL
         AND swap_state IS NULL AND NOT EXISTS ({NESTING_OTHER}) AND NOT EXISTS ({LIVE_RUN})"
    ))
    .bind(target)
    .bind(pack_id)
    .execute(pool)
    .await?;
    if moved.rows_affected() == 1 {
        return Ok(Ok(()));
    }
    let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM import_packs WHERE id = ?")
        .bind(pack_id)
        .fetch_optional(pool)
        .await?;
    if exists.is_none() {
        return Ok(Err(TargetRefused::NoPack));
    }
    let other: Option<String> = sqlx::query_scalar(NESTING_OTHER)
        .bind(target)
        .bind(pack_id)
        .fetch_optional(pool)
        .await?;
    if let Some(other) = other {
        return Ok(Err(TargetRefused::Nests(other)));
    }
    let running: Option<i64> = sqlx::query_scalar(LIVE_RUN)
        .bind(target)
        .bind(pack_id)
        .fetch_optional(pool)
        .await?;
    Ok(Err(if running.is_some() {
        TargetRefused::Running
    } else {
        TargetRefused::Built
    }))
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
    pub original_sha256: String,
    pub kind: String,
    /// The Kenney store page lookup's answer, `metadata::StorePage` as JSON, `None` before one.
    pub store_page: Option<String>,
    /// Enabled steps in position order, as `(kind, options)`.
    pub steps: Vec<(String, Value)>,
}

pub async fn recipe(pool: &Pool<Sqlite>, pack_id: &str) -> sqlx::Result<Recipe> {
    #[allow(clippy::type_complexity)]
    let (
        target,
        recipe_rev,
        current_build,
        swap_state,
        original_path,
        size,
        mtime,
        sha256,
        kind,
        page,
    ): (
        String,
        i64,
        Option<String>,
        Option<String>,
        String,
        i64,
        i64,
        String,
        String,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT p.target, p.recipe_rev, p.current_build, p.swap_state, i.original_path,
         i.original_size, i.original_mtime_ns, i.original_sha256, i.kind, p.store_page
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
        original_sha256: sha256,
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
    pub origin: Origin,
}

/// Build `build_id`'s manifest rows, sorted by path.
pub async fn manifest(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    build_id: &str,
) -> sqlx::Result<Vec<ManifestRow>> {
    let rows: Vec<(String, i64, i64, String, Option<String>, String)> = sqlx::query_as(
        "SELECT path, size, mtime_ns, sha256, source_path, origin
         FROM import_outputs WHERE pack_id = ? AND build_id = ? ORDER BY path",
    )
    .bind(pack_id)
    .bind(build_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(path, size, mtime_ns, sha256, source_path, origin)| ManifestRow {
                path,
                size,
                mtime_ns,
                sha256,
                source_path,
                origin: Origin::parse(&origin),
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
    /// Why a step kind can't run here, by kind: `unity_glb` without an accepted Blender.
    pub unavailable: std::collections::BTreeMap<String, String>,
    /// The current build's kept files, by path relative to the library root: not from the
    /// original, so a rebuild carries them but can't recreate them.
    pub kept: Vec<String>,
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
    let kept = sqlx::query_scalar(
        "SELECT o.path FROM import_outputs o JOIN import_packs p ON p.id = o.pack_id
         AND o.build_id = p.current_build WHERE o.pack_id = ? AND o.origin = 'kept'
         ORDER BY o.path",
    )
    .bind(pack_id)
    .fetch_all(pool)
    .await?;
    Ok(Some(PackSteps {
        recipe_rev,
        steps,
        unavailable: Default::default(),
        kept,
    }))
}

/// Why a pack's steps can't be edited, or it can't be rebuilt, now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    NoPack,
    /// A run for the pack is queued or running, or a swap or Remove is journaled.
    Busy,
    /// The pack's steps changed since the caller read them: its `recipe_rev` moved on.
    Stale,
}

/// The `import_packs` row is busy: a run for it is queued or running, or its `swap_state` is set.
pub(super) const BUSY: &str = "(import_packs.swap_state IS NOT NULL OR EXISTS (
    SELECT 1 FROM import_runs r, json_each(r.packs) j
    WHERE r.status IN ('queued', 'running')
    AND json_extract(j.value, '$.packId') = import_packs.id))";

async fn refusal(tx: &mut SqliteConnection, pack_id: &str) -> sqlx::Result<Refused> {
    let busy: Option<bool> =
        sqlx::query_scalar(&format!("SELECT {BUSY} FROM import_packs WHERE id = ?"))
            .bind(pack_id)
            .fetch_optional(&mut *tx)
            .await?;
    Ok(match busy {
        None => Refused::NoPack,
        Some(true) => Refused::Busy,
        Some(false) => Refused::Stale,
    })
}

/// Replaces pack `pack_id`'s steps with `steps` and bumps its `recipe_rev`, in one transaction
/// whose first write checks that no run for the pack is queued or running and, given `read_at`,
/// that the steps are still at the `recipe_rev` they were read at, so an edit made since (an
/// in-app rename's step) is never overwritten. Returns the new `recipe_rev`.
pub async fn set_steps(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    steps: &[Step],
    read_at: Option<i64>,
) -> sqlx::Result<Result<i64, Refused>> {
    let mut tx = pool.begin().await?;
    let bumped: Option<i64> = sqlx::query_scalar(&format!(
        "UPDATE import_packs SET recipe_rev = recipe_rev + 1 WHERE id = ? AND NOT {BUSY}
         AND (?2 IS NULL OR recipe_rev = ?2) RETURNING recipe_rev"
    ))
    .bind(pack_id)
    .bind(read_at)
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

/// The recipe edits a Rebuild's drift choices make (`drift::edits`): an `exclude` for each
/// missing file left out, the enabled `exclude` of each one restored turned off, and the
/// metadata step turned off or on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriftEdits {
    pub excludes: Vec<Vec<String>>,
    pub includes: Vec<Vec<String>>,
    pub metadata_off: bool,
    pub metadata_on: bool,
}

impl DriftEdits {
    /// The source paths (JSON arrays) of the enabled `exclude` steps once these edits are made,
    /// given the ones enabled now: without those turned back, with those recorded.
    pub fn excluded_after(
        &self,
        now: &std::collections::HashSet<String>,
    ) -> std::collections::HashSet<String> {
        let back: std::collections::HashSet<String> =
            self.includes.iter().map(|s| source_json(s)).collect();
        now.iter()
            .filter(|s| !back.contains(*s))
            .cloned()
            .chain(self.excludes.iter().map(|s| source_json(s)))
            .collect()
    }

    fn is_empty(&self) -> bool {
        self.excludes.is_empty()
            && self.includes.is_empty()
            && !self.metadata_off
            && !self.metadata_on
    }
}

/// Queues a rebuild of pack `pack_id`, owned by `owner`, with its drift choices `drift` (the
/// JSON array `import_runs.drift` holds under the pack's id), unless a run for it is queued or
/// running. In the same transaction it first makes the choices' recipe `edits`, bumping
/// `recipe_rev`, and the run is queued at the `recipe_rev` they leave. Returns the run's id.
pub async fn queue_rebuild(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    owner: &Owner,
    now: i64,
    drift: &Value,
    edits: &DriftEdits,
) -> sqlx::Result<Result<String, Refused>> {
    let run_id = uuid::Uuid::new_v4().to_string();
    let mut tx = pool.begin().await?;
    if !edits.is_empty() {
        let bumped: Option<i64> = sqlx::query_scalar(&format!(
            "UPDATE import_packs SET recipe_rev = recipe_rev + 1 WHERE id = ? AND NOT {BUSY}
             RETURNING recipe_rev"
        ))
        .bind(pack_id)
        .fetch_optional(&mut *tx)
        .await?;
        if bumped.is_none() {
            return Ok(Err(refusal(&mut tx, pack_id).await?));
        }
        record_drift_edits(&mut tx, pack_id, edits).await?;
    }
    let queued = sqlx::query(&format!(
        "INSERT INTO import_runs (id, kind, packs, drift, status, created_at, owner_machine,
         owner_boot, owner_pid, owner_started, heartbeat_at)
         SELECT ?, 'rebuild', json_array(json_object('packId', id, 'recipeRev', recipe_rev)),
         json_object(id, json(?)), 'queued', ?, ?, ?, ?, ?, ? FROM import_packs
         WHERE id = ? AND NOT {BUSY}"
    ))
    .bind(&run_id)
    .bind(drift.to_string())
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

/// Appends an `exclude` for each source of `edits` no enabled `exclude` names yet, but a `.meta`
/// whose asset is excluded too (its asset's step drops it), and turns the metadata step off.
async fn record_drift_edits(
    tx: &mut SqliteConnection,
    pack_id: &str,
    edits: &DriftEdits,
) -> sqlx::Result<()> {
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT json_extract(options, '$.source') FROM import_steps
         WHERE pack_id = ? AND kind = 'exclude' AND enabled = 1",
    )
    .bind(pack_id)
    .fetch_all(&mut *tx)
    .await?;
    let mut excluded: Vec<Vec<String>> = existing
        .iter()
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect();
    // A `.meta` is covered by its asset's exclude as the steps will be: one this Rebuild turns
    // back covers nothing.
    let now = excluded.iter().map(|s| source_json(s)).collect();
    let after = edits.excluded_after(&now);
    let covered = |source: &Vec<String>| {
        asset_of_meta(&source_json(source)).is_some_and(|asset| after.contains(&asset))
    };
    let new: Vec<Vec<String>> = edits
        .excludes
        .iter()
        .filter(|source| !covered(source))
        .cloned()
        .collect();
    let mut position: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(position) + 1, 0) FROM import_steps WHERE pack_id = ?",
    )
    .bind(pack_id)
    .fetch_one(&mut *tx)
    .await?;
    for source in new {
        if excluded.contains(&source) {
            continue;
        }
        let step = StepRecord::place(&PlaceStep::Exclude {
            source: source.clone(),
        });
        sqlx::query(
            "INSERT INTO import_steps (pack_id, position, kind, enabled, options)
             VALUES (?, ?, ?, 1, ?)",
        )
        .bind(pack_id)
        .bind(position)
        .bind(&step.kind)
        .bind(step.options.to_string())
        .execute(&mut *tx)
        .await?;
        position += 1;
        excluded.push(source);
    }
    for source in &edits.includes {
        sqlx::query(
            "UPDATE import_steps SET enabled = 0 WHERE pack_id = ? AND kind = 'exclude'
             AND enabled = 1 AND json_extract(options, '$.source') = ?",
        )
        .bind(pack_id)
        .bind(source_json(source))
        .execute(&mut *tx)
        .await?;
    }
    if edits.metadata_off || edits.metadata_on {
        sqlx::query("UPDATE import_steps SET enabled = ? WHERE pack_id = ? AND kind = 'metadata'")
            .bind(edits.metadata_on)
            .bind(pack_id)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

/// The drift choices of pack `pack_id`'s latest rebuild that never committed (failed,
/// cancelled or interrupted), queued since the pack's last build and with its steps still at
/// the `recipe_rev` that run left: the recipe edits those choices made are still its own, and a
/// Restore may turn them back. Empty when there is no such run.
pub async fn uncommitted(pool: &Pool<Sqlite>, pack_id: &str) -> sqlx::Result<Value> {
    let drift: Option<Option<String>> = sqlx::query_scalar(
        "SELECT json_extract(r.drift, '$.' || json_quote(p.id))
         FROM import_runs r, json_each(r.packs) j, import_packs p
         WHERE p.id = ? AND json_extract(j.value, '$.packId') = p.id AND r.kind = 'rebuild'
         AND r.status IN ('failed', 'cancelled', 'interrupted')
         AND json_extract(j.value, '$.recipeRev') = p.recipe_rev
         AND r.created_at >= COALESCE(p.built_at, 0)
         ORDER BY r.created_at DESC, r.rowid DESC LIMIT 1",
    )
    .bind(pack_id)
    .fetch_optional(pool)
    .await?;
    Ok(drift
        .flatten()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_else(|| json!([])))
}

/// The drift choices run `run_id` was queued with for pack `pack_id`, as the JSON array
/// `queue_rebuild` stored (empty for an import's first build).
pub async fn run_drift(pool: &Pool<Sqlite>, run_id: &str, pack_id: &str) -> sqlx::Result<Value> {
    let drift: Option<String> = sqlx::query_scalar(
        "SELECT json_extract(drift, '$.' || json_quote(?)) FROM import_runs WHERE id = ?",
    )
    .bind(pack_id)
    .bind(run_id)
    .fetch_one(pool)
    .await?;
    Ok(drift
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_else(|| json!([])))
}

/// Writes `outputs` as build `build_id`'s manifest rows in 200-row transactions. They mean
/// nothing until the build commits. A kept file without a source path, one added outside
/// OpenDAM, is stored with a `NULL` source path, so it is never carried by a file key.
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
            .bind(output.origin.as_str())
            .bind(output.stored_source())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn output(origin: Origin, source: &[&str]) -> Output {
        Output {
            path: "P/a".into(),
            size: 1,
            mtime_ns: 1,
            sha256: String::new(),
            source_path: source.iter().map(|s| s.to_string()).collect(),
            origin,
        }
    }

    #[test]
    fn only_a_kept_file_of_its_own_has_no_stored_source_nor_file_key() {
        assert_eq!(output(Origin::Kept, &[]).stored_source(), None);
        assert_eq!(output(Origin::Kept, &[]).file_key(), None);
        // The metadata step's datapackage.json comes from no entry, and still has its key.
        assert_eq!(
            output(Origin::Generated, &[]).file_key(),
            Some(("[]".into(), true))
        );
        assert_eq!(
            output(Origin::Kept, &["Assets/a.png"]).file_key(),
            Some((r#"["Assets/a.png"]"#.into(), false)),
            "a kept copy has the replaced file's key"
        );
    }

    #[test]
    fn a_meta_is_left_out_by_its_own_exclude_or_its_asset_s() {
        let meta = source_json(&["Pack.unitypackage".into(), "Assets/a.png.meta".into()]);
        let asset = source_json(&["Pack.unitypackage".into(), "Assets/a.png".into()]);
        assert_eq!(asset_of_meta(&meta), Some(asset.clone()));
        assert_eq!(asset_of_meta(&asset), None);
        assert!(leaves_out(&[asset.clone()].into(), &meta));
        assert!(leaves_out(&[meta.clone()].into(), &meta));
        assert!(
            !leaves_out(&[meta].into(), &asset),
            "a .meta's exclude drops only it"
        );
    }
}
