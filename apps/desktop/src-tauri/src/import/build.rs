//! A pack's first build (plan pack-import, Flow). It stages the whole pack beside its target as
//! `.<Pack>.import-<build id>`, writing the staged files' manifest rows under the new build id,
//! then:
//!
//! 1. **Lock.** Takes the library's `index_lock`, then the advisory `.opendam/index.lock`, and
//!    holds both to the end. Cancel takes effect only before this point.
//! 2. **Re-check.** Stops when the recipe, the target or the staging folder changed meanwhile.
//! 3. **Journal.** Sets `swap_state = 'swapping'` with the build id and `swap_prev`.
//! 4. **Swap.** Renames the staging folder in (`fs_swap`).
//! 5. **Index without pruning** (`Prune::Defer`), journaling every row it inserts.
//! 6. **Commit.** Flips `current_build` to the build, in one transaction.
//! 7. **Settle.** Indexes the pack with `Prune::Now`, which ends with the structure pass, and
//!    deletes the other builds' rows and this build's journal.
//!
//! Step 5 or 6 failing rolls the swap back: the pack folder goes, the inserted rows with it, and
//! `current_build` stays as it was. A first build carries no row (there is no previous build to
//! carry from), so its journal holds only inserted rows.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::{Pool, Sqlite};

use super::place::{place, PlaceStep};
use super::records::{self, InsertedRows, Output, Recipe};
use super::unitypackage::{extract_unitypackage, SkippedEntry};
use crate::db::{Database, LibraryHandle};
use crate::fs_swap;
use crate::watcher::{self, IndexLock, Prune};

/// What Cancel answers once a build holds the lock.
pub const FINISHING: &str = "Finishing, can't cancel now";
const CHANGED: &str = "The pack changed during the build; Rebuild again";

/// `fs_swap` tags of the folders beside the pack.
const STAGING: &str = "import";
const PREV: &str = "prev";
const FAILED: &str = "failed";

/// Whether a run was cancelled, and whether its build passed the point where Cancel stops
/// working (step 1). One mutex for both, so a Cancel never lands after that point unnoticed.
#[derive(Default)]
pub struct RunControl {
    state: Mutex<(bool, bool)>,
}

impl RunControl {
    /// Cancels the run, unless its build already holds the lock.
    pub fn cancel(&self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.1 {
            return Err(FINISHING.into());
        }
        state.0 = true;
        Ok(())
    }

    pub fn cancelled(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0
    }

    /// Marks the build locked, past Cancel. False when it was cancelled first.
    fn lock(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.1 = !state.0;
        state.1
    }
}

/// Where a test can pause a build or make it fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// Staged, its manifest rows written, before step 1.
    Staged,
    /// Step 5, after the index.
    Indexed,
    /// Step 4's rename of the staging folder to the live name (failure only).
    MoveIn,
}

/// Test hooks, as autotag undo's `Crashes`. They do nothing outside tests.
#[derive(Default)]
pub struct Hooks {
    #[cfg(test)]
    points: Mutex<Vec<(Point, Hook)>>,
}

#[cfg(test)]
#[derive(Clone)]
enum Hook {
    Pause(Arc<Pause>),
    Fail,
}

/// A paused build: it notifies `reached` and waits for `release`.
#[cfg(test)]
#[derive(Default)]
pub struct Pause {
    pub reached: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

impl Hooks {
    #[cfg(test)]
    pub fn pause(&self, point: Point) -> Arc<Pause> {
        let pause = Arc::new(Pause::default());
        self.points
            .lock()
            .unwrap()
            .push((point, Hook::Pause(pause.clone())));
        pause
    }

    #[cfg(test)]
    pub fn fail(&self, point: Point) {
        self.points.lock().unwrap().push((point, Hook::Fail));
    }

    #[cfg(test)]
    fn hooks(&self, point: Point) -> Vec<Hook> {
        let points = self.points.lock().unwrap();
        points
            .iter()
            .filter(|(p, _)| *p == point)
            .map(|(_, h)| h.clone())
            .collect()
    }

    fn fails(&self, _point: Point) -> bool {
        #[cfg(test)]
        if self.hooks(_point).iter().any(|h| matches!(h, Hook::Fail)) {
            return true;
        }
        false
    }

    /// Pauses at `point` if a test asked to, then fails if it asked to.
    async fn at(&self, point: Point) -> Result<(), String> {
        #[cfg(test)]
        for hook in self.hooks(point) {
            if let Hook::Pause(pause) = hook {
                pause.reached.notify_one();
                pause.release.notified().await;
            }
        }
        if self.fails(point) {
            return Err(format!("injected failure at {point:?}"));
        }
        Ok(())
    }
}

/// A build's progress, for `import:progress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub step: &'static str,
    pub done: u64,
    pub total: u64,
}

pub type ProgressSink = Arc<dyn Fn(Progress) + Send + Sync>;

/// What a build needs from its run.
pub struct BuildEnv<'a> {
    pub db: &'a Database,
    pub library: &'a LibraryHandle,
    pub control: Arc<RunControl>,
    pub hooks: &'a Hooks,
    pub progress: ProgressSink,
    /// How long step 1 waits for another holder of the advisory lock.
    pub lock_wait: Duration,
}

/// Why a build stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// Cancelled before step 1: nothing changed.
    Cancelled,
    Failed(String),
}

impl From<String> for Stop {
    fn from(message: String) -> Self {
        Stop::Failed(message)
    }
}

fn db_error(e: sqlx::Error) -> Stop {
    Stop::Failed(format!("Database error: {e}"))
}

/// The steps a build runs, from its recipe.
struct Plan {
    subpath: Option<String>,
    place: Vec<PlaceStep>,
}

impl Plan {
    fn of(recipe: &Recipe) -> Result<Self, String> {
        if recipe.kind != "unitypackage" {
            return Err(format!("{} originals can't be built yet", recipe.kind));
        }
        let mut subpath = None;
        let mut extracts = 0;
        let mut place = Vec::new();
        for (kind, options) in &recipe.steps {
            match kind.as_str() {
                "extract_unitypackage" => {
                    extracts += 1;
                    subpath = options["subpath"].as_str().map(str::to_string);
                }
                "flatten" | "hoist" | "rename" | "exclude" => {
                    let mut step = options.clone();
                    step["kind"] = kind.clone().into();
                    place.push(
                        serde_json::from_value(step)
                            .map_err(|e| format!("The {kind} step's options are invalid: {e}"))?,
                    );
                }
                // Its datapackage.json comes with the metadata step's own work (plan Pack
                // metadata). Until then the step only records the import's publisher.
                "metadata" => {}
                other => return Err(format!("The {other} step can't run yet")),
            }
        }
        if extracts != 1 {
            return Err("A pack needs exactly one extract step".into());
        }
        Ok(Plan { subpath, place })
    }
}

/// Builds pack `pack_id`, queued at `queued_rev`, for the first time. Fills `warnings` with the
/// entries the extraction skipped.
pub async fn build_first(
    env: &BuildEnv<'_>,
    pack_id: &str,
    queued_rev: i64,
    warnings: &mut Vec<SkippedEntry>,
) -> Result<(), Stop> {
    let pool = &env.library.pool;
    let recipe = records::recipe(pool, pack_id).await.map_err(db_error)?;
    if recipe.current_build.is_some() {
        return Err(Stop::Failed(
            "The pack is already built, and rebuilding isn't supported yet".to_string(),
        ));
    }
    if recipe.swap_state.is_some() || recipe.recipe_rev != queued_rev {
        return Err(Stop::Failed(CHANGED.to_string()));
    }
    let plan = Plan::of(&recipe)?;
    let root = env.library.root_path.clone();
    let live = root.join(&recipe.target);
    let build_id = uuid::Uuid::new_v4().to_string();
    let created = missing_folders(&root, &live);
    let staging = fs_swap::create_staging(&live, STAGING, &build_id).map_err(|e| {
        remove_created(&created);
        format!(
            "Could not create the staging folder beside {}: {e}",
            live.display()
        )
    })?;
    let cleanup = Cleanup {
        pool,
        pack_id,
        build_id: &build_id,
        staging: &staging,
        created: &created,
    };

    let outputs = match stage(env, &recipe, &plan, &staging, &build_id, warnings).await {
        Ok(outputs) => outputs,
        Err(stop) => return Err(cleanup.run(stop).await),
    };

    // ── 1. Lock ─────────────────────────────────────────────────────────
    (env.progress)(Progress {
        step: "waiting for the library",
        done: 0,
        total: 0,
    });
    let index_lock = env.db.index_lock(&env.library.id);
    let index_guard = index_lock.lock().await;
    let advisory = match advisory_lock(env, &root).await {
        Ok(advisory) => advisory,
        Err(stop) => return Err(cleanup.run(stop).await),
    };
    if !env.control.lock() {
        return Err(cleanup.run(Stop::Cancelled).await);
    }

    // ── 2. Re-check ─────────────────────────────────────────────────────
    if let Err(stop) = recheck(
        pool, &recipe, queued_rev, &live, &staging, &build_id, &outputs,
    )
    .await
    {
        return Err(cleanup.run(stop).await);
    }
    if !advisory.still_held() {
        let lost = "Another machine took the library's index lock; Rebuild again".to_string();
        return Err(cleanup.run(lost.into()).await);
    }

    // ── 3. Journal ──────────────────────────────────────────────────────
    let swap_prev = live.exists();
    let journaled = sqlx::query(
        "UPDATE import_packs SET swap_state = 'swapping', swap_build = ?, swap_prev = ?
         WHERE id = ? AND swap_state IS NULL",
    )
    .bind(&build_id)
    .bind(swap_prev)
    .bind(pack_id)
    .execute(pool)
    .await;
    if !matches!(&journaled, Ok(done) if done.rows_affected() == 1) {
        return Err(cleanup.run(CHANGED.to_string().into()).await);
    }

    // ── 4. Swap ─────────────────────────────────────────────────────────
    (env.progress)(Progress {
        step: "swapping",
        done: 0,
        total: 0,
    });
    let prev = swap_prev.then(|| fs_swap::sibling(&live, PREV, &build_id));
    let swapped = fs_swap::swap(
        &live,
        &staging,
        prev.as_deref(),
        fs_swap::Old::Keep,
        |a, b| {
            if b == live && env.hooks.fails(Point::MoveIn) {
                return Err(std::io::Error::other("injected failure"));
            }
            std::fs::rename(a, b)
        },
    );
    match swapped {
        Ok(()) => {}
        Err(fs_swap::SwapError::In {
            error,
            restored: Some(Err(back)),
        }) => {
            // The journal stays: recovery puts the previous folder back.
            return Err(Stop::Failed(format!(
                "Could not move the built pack to {}: {error}, nor the previous folder back: {back}",
                live.display()
            )));
        }
        Err(fs_swap::SwapError::Aside(error) | fs_swap::SwapError::In { error, .. }) => {
            let failed = format!(
                "Could not move the built pack to {}: {error}",
                live.display()
            );
            if let Err(e) = unjournal(pool, pack_id, &build_id).await {
                return Err(Stop::Failed(format!(
                    "{failed}. Clearing its journal failed: {e}"
                )));
            }
            return Err(cleanup.run(failed.into()).await);
        }
    }

    // ── 5. Index without pruning, 6. Commit ─────────────────────────────
    (env.progress)(Progress {
        step: "indexing",
        done: 0,
        total: 0,
    });
    let pack_path = [live.to_string_lossy().into_owned()];
    let committed = async {
        let journal = InsertedRows::after(pool, pack_id, &build_id)
            .await
            .map_err(|e| e.to_string())?;
        watcher::index_paths_locked(
            env.db,
            &env.library.id,
            &pack_path,
            Prune::Defer(&journal),
            &index_guard,
            &advisory,
        )
        .await
        .map_err(|e| format!("Indexing the pack failed: {e:#}"))?;
        env.hooks.at(Point::Indexed).await?;
        sqlx::query(
            "UPDATE import_packs SET current_build = ?, status = 'built', built_at = ?,
             swap_state = NULL, swap_build = NULL, swap_prev = NULL WHERE id = ?",
        )
        .bind(&build_id)
        .bind(chrono::Utc::now().timestamp_millis())
        .bind(pack_id)
        .execute(pool)
        .await
        .map_err(|e| format!("Committing the build failed: {e}"))?;
        Ok::<(), String>(())
    }
    .await;
    if let Err(error) = committed {
        let swap = Swapped {
            live: &live,
            prev: prev.as_deref(),
            build_id: &build_id,
            created: &created,
        };
        return Err(rollback(env, &index_guard, &advisory, pack_id, &swap, error).await);
    }

    // ── 7. Settle ───────────────────────────────────────────────────────
    (env.progress)(Progress {
        step: "settling",
        done: 0,
        total: 0,
    });
    if let Err(e) = watcher::index_paths_locked(
        env.db,
        &env.library.id,
        &pack_path,
        Prune::Now,
        &index_guard,
        &advisory,
    )
    .await
    {
        // Committed: the next refresh prunes and derives the structure instead.
        tracing::warn!("Settling the build of {}: {e:#}", recipe.target);
    }
    if let Some(prev) = &prev {
        fs_swap::discard(prev);
    }
    let settled = async {
        let mut tx = pool.begin().await?;
        sqlx::query("DELETE FROM import_outputs WHERE pack_id = ? AND build_id != ?")
            .bind(pack_id)
            .bind(&build_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM import_moves WHERE pack_id = ? AND build_id = ?")
            .bind(pack_id)
            .bind(&build_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }
    .await;
    if let Err(e) = settled {
        tracing::warn!("Settling the records of {}: {e}", recipe.target);
    }
    Ok(())
}

/// The folders between `root` and `live` (exclusive) that don't exist yet, deepest first, so a
/// build that stops can remove the ones it made.
fn missing_folders(root: &Path, live: &Path) -> Vec<PathBuf> {
    live.ancestors()
        .skip(1)
        .take_while(|dir| *dir != root && dir.starts_with(root))
        .filter(|dir| !dir.exists())
        .map(Path::to_path_buf)
        .collect()
}

fn remove_created(created: &[PathBuf]) {
    for dir in created {
        // Fails, as it should, once something else is in it
        let _ = std::fs::remove_dir(dir);
    }
}

/// Undoes a build that stopped before its swap: deletes its staging folder and manifest rows,
/// and the folders it made.
struct Cleanup<'a> {
    pool: &'a Pool<Sqlite>,
    pack_id: &'a str,
    build_id: &'a str,
    staging: &'a Path,
    created: &'a [PathBuf],
}

impl Cleanup<'_> {
    async fn run(&self, stop: Stop) -> Stop {
        fs_swap::discard(self.staging);
        remove_created(self.created);
        let deleted = async {
            let mut tx = self.pool.begin().await?;
            records::delete_build(&mut tx, self.pack_id, self.build_id).await?;
            tx.commit().await
        }
        .await;
        if let Err(e) = deleted {
            tracing::warn!("Could not delete the rows of build {}: {e}", self.build_id);
        }
        stop
    }
}

/// Extracts and places the pack into `staging`, hashes its files and writes their manifest
/// rows under `build_id`, checking for Cancel between steps.
async fn stage(
    env: &BuildEnv<'_>,
    recipe: &Recipe,
    plan: &Plan,
    staging: &Path,
    build_id: &str,
    warnings: &mut Vec<SkippedEntry>,
) -> Result<Vec<Output>, Stop> {
    let cancelled = || {
        if env.control.cancelled() {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    };
    let original = PathBuf::from(&recipe.original_path);
    let (size, mtime_ns) = file_stamp(&original)
        .map_err(|e| format!("The original {} can't be read: {e}", original.display()))?;
    if size != recipe.original_size as u64 || mtime_ns != recipe.original_mtime_ns {
        return Err(Stop::Failed(format!(
            "The original {} changed since it was imported",
            original.display()
        )));
    }

    (env.progress)(Progress {
        step: "extracting",
        done: 0,
        total: 0,
    });
    let (to, subpath) = (staging.to_path_buf(), plan.subpath.clone());
    let extraction = blocking(move || {
        extract_unitypackage(&original, &to, subpath.as_deref()).map_err(|e| format!("{e:#}"))
    })
    .await?;
    warnings.extend(extraction.skipped);
    cancelled()?;

    (env.progress)(Progress {
        step: "placing",
        done: 0,
        total: 0,
    });
    let (to, steps, mut files) = (staging.to_path_buf(), plan.place.clone(), extraction.files);
    let files = blocking(move || {
        place(&to, &mut files, &steps).map_err(|e| format!("{e:#}"))?;
        Ok(files)
    })
    .await?;
    cancelled()?;

    let (to, target, control, progress) = (
        staging.to_path_buf(),
        recipe.target.clone(),
        env.control.clone(),
        env.progress.clone(),
    );
    let outputs = tokio::task::spawn_blocking(move || {
        let total = files.len() as u64;
        let mut outputs = Vec::with_capacity(files.len());
        for (done, file) in files.into_iter().enumerate() {
            if control.cancelled() {
                return Err(Stop::Cancelled);
            }
            progress(Progress {
                step: "hashing",
                done: done as u64,
                total,
            });
            let path = to.join(&file.path);
            let read = |e: std::io::Error| format!("The staged {} can't be read: {e}", file.path);
            let (size, mtime_ns) = file_stamp(&path).map_err(read)?;
            outputs.push(Output {
                path: format!("{target}/{}", file.path),
                size,
                mtime_ns,
                sha256: crate::export::hashes::sha256_file(&path).map_err(read)?,
                source_path: file.source_path,
            });
        }
        Ok(outputs)
    })
    .await
    .map_err(|e| Stop::Failed(format!("The build's worker stopped: {e}")))??;
    cancelled()?;

    records::write_outputs(&env.library.pool, &recipe.pack_id, build_id, &outputs)
        .await
        .map_err(db_error)?;
    env.hooks.at(Point::Staged).await?;
    cancelled()?;
    Ok(outputs)
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, Stop> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("The build's worker stopped: {e}"))?
        .map_err(Stop::Failed)
}

/// A file's size and mtime in nanoseconds.
fn file_stamp(path: &Path) -> std::io::Result<(u64, i64)> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Ok((meta.len(), mtime.as_nanos() as i64))
}

/// Takes the advisory lock, waiting up to `lock_wait` for another holder. Cancel still stops the
/// wait. A lock taken at once is returned even when cancelled: `RunControl::lock` decides then.
async fn advisory_lock(env: &BuildEnv<'_>, root: &Path) -> Result<IndexLock, Stop> {
    let deadline = tokio::time::Instant::now() + env.lock_wait;
    loop {
        if let Some(lock) = IndexLock::try_acquire(root).await {
            return Ok(lock);
        }
        if env.control.cancelled() {
            return Err(Stop::Cancelled);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Stop::Failed(
                "Another machine is indexing this library; Rebuild again later".to_string(),
            ));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Step 2: the recipe is as queued, the target still doesn't exist, and the staging folder and
/// the build's manifest rows hold exactly the files the build staged.
async fn recheck(
    pool: &Pool<Sqlite>,
    queued: &Recipe,
    queued_rev: i64,
    live: &Path,
    staging: &Path,
    build_id: &str,
    outputs: &[Output],
) -> Result<(), Stop> {
    let now = records::recipe(pool, &queued.pack_id)
        .await
        .map_err(db_error)?;
    let same_recipe = now.recipe_rev == queued_rev
        && now.current_build.is_none()
        && now.swap_state.is_none()
        && now.target == queued.target;
    let mut expected: Vec<(String, i64)> = outputs
        .iter()
        .map(|o| (o.path.clone(), o.size as i64))
        .collect();
    expected.sort();
    let prefix = format!("{}/", queued.target);
    let staged = staged_sizes(staging).map(|files| {
        files
            .into_iter()
            .map(|(path, size)| (format!("{prefix}{path}"), size))
            .collect::<Vec<_>>()
    });
    let rows = records::output_sizes(pool, &queued.pack_id, build_id)
        .await
        .map_err(db_error)?;
    if !same_recipe
        || live.symlink_metadata().is_ok()
        || staged.ok().as_ref() != Some(&expected)
        || rows != expected
    {
        return Err(Stop::Failed(CHANGED.to_string()));
    }
    Ok(())
}

/// Every file below `folder` as `(path, size)`, sorted. Anything but a regular file or a folder
/// is an error.
fn staged_sizes(folder: &Path) -> std::io::Result<Vec<(String, i64)>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(folder).min_depth(1) {
        let entry = entry?;
        let meta = entry.path().symlink_metadata()?;
        if meta.is_dir() {
            continue;
        }
        if !meta.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        let rel = entry.path().strip_prefix(folder).expect("below the folder");
        files.push((rel.to_string_lossy().into_owned(), meta.len() as i64));
    }
    files.sort();
    Ok(files)
}

/// Clears a swap's journal and the build's rows, when nothing of the build is in place.
async fn unjournal(pool: &Pool<Sqlite>, pack_id: &str, build_id: &str) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE import_packs SET swap_state = NULL, swap_build = NULL, swap_prev = NULL
         WHERE id = ?",
    )
    .bind(pack_id)
    .execute(&mut *tx)
    .await?;
    records::delete_build(&mut tx, pack_id, build_id).await?;
    tx.commit().await
}

/// A build swapped in and not committed.
struct Swapped<'a> {
    live: &'a Path,
    prev: Option<&'a Path>,
    build_id: &'a str,
    created: &'a [PathBuf],
}

/// Rolls back a swap that failed before its commit (plan Flow, Rollback), still holding both
/// locks, and returns the run's failure. A step that fails leaves the journal set, for recovery.
async fn rollback(
    env: &BuildEnv<'_>,
    index_guard: &tokio::sync::MutexGuard<'_, ()>,
    advisory: &IndexLock,
    pack_id: &str,
    swap: &Swapped<'_>,
    error: String,
) -> Stop {
    let pool = &env.library.pool;
    let unfinished = |step: String| {
        tracing::error!("Rolling back build {}: {step}", swap.build_id);
        Stop::Failed(format!(
            "{error}. Rolling it back failed ({step}), so it is finished when the library next opens"
        ))
    };

    // 1-2. The failed build aside, the previous folder (if any) back.
    let failed = fs_swap::sibling(swap.live, FAILED, swap.build_id);
    if let Err(e) = std::fs::rename(swap.live, &failed) {
        return unfinished(format!("moving the pack aside: {e}"));
    }
    if let Some(prev) = swap.prev {
        if let Err(e) = std::fs::rename(prev, swap.live) {
            return unfinished(format!("moving the previous folder back: {e}"));
        }
    }

    // 3. The rows the build's index inserted, then the structure of what is on disk now.
    let deleted = sqlx::query(
        "DELETE FROM assets WHERE id IN (SELECT asset_id FROM import_moves
         WHERE pack_id = ? AND build_id = ? AND kind = 'inserted')",
    )
    .bind(pack_id)
    .bind(swap.build_id)
    .execute(pool)
    .await;
    if let Err(e) = deleted {
        return unfinished(format!("deleting the rows it inserted: {e}"));
    }
    if swap.prev.is_some() {
        let pack_path = [swap.live.to_string_lossy().into_owned()];
        let indexed = watcher::index_paths_locked(
            env.db,
            &env.library.id,
            &pack_path,
            Prune::Now,
            index_guard,
            advisory,
        )
        .await;
        if let Err(e) = indexed {
            tracing::warn!("Re-indexing the restored pack: {e:#}");
        }
    } else {
        // No pack folder now, as before the build: an index would prune rows it never should.
        crate::structure::rebuild_library_locked(env.db, env.library, index_guard, advisory).await;
    }

    // 4. The journal and the build's records in one transaction, then its folder.
    if let Err(e) = unjournal(pool, pack_id, swap.build_id).await {
        return unfinished(format!("clearing its journal: {e}"));
    }
    fs_swap::discard(&failed);
    remove_created(swap.created);
    Stop::Failed(error)
}
