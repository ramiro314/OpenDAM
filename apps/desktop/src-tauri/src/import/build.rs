//! A pack's build, first or later (plan pack-import, Flow). It stages the whole pack beside its
//! target as `.<Pack>.import-<build id>`: extract, place, then convert (`unity_glb`, whose GLBs
//! are `generated` rows with their models' source paths), writing the staged files' manifest
//! rows under the new build id. An Unreal pack's extract step, `export_unreal` (`unreal_glb`),
//! writes GLBs and docs, its extracted files. A staged file the previous build also had, by file
//! key, size and sha256, gets that build's mtime, so the index sees it unchanged, moved or not,
//! and queues no preview. Then:
//!
//! 1. **Lock.** Takes the library's `index_lock`, then the advisory `.opendam/index.lock`, and
//!    holds both to the end. Cancel takes effect only before this point.
//! 2. **Re-check.** Stops when the recipe, the pack folder (against the current manifest) or the
//!    staging folder changed meanwhile.
//! 3. **Journal.** Sets `swap_state = 'swapping'` with the build id and `swap_prev`.
//! 4. **Swap.** Moves the live folder aside as `.<Pack>.prev-<build id>`, if there is one, and
//!    renames the staging folder in (`fs_swap`).
//! 5. **Carry, then index without pruning.** Carries the rows of moved files to their new paths
//!    (`carry`), then indexes with `Prune::Defer`, journaling every row it inserts.
//! 6. **Commit.** Flips `current_build` to the build, in one transaction.
//! 7. **Settle.** Indexes the pack with `Prune::Now`, which prunes the rows of paths only the
//!    previous build had and ends with the structure pass, then deletes the previous folder, the
//!    other builds' rows and this build's journal.
//!
//! Step 5 or 6 failing rolls the swap back: the previous folder (if any) returns, the inserted
//! rows go, the carried rows go back by asset id, and `current_build` stays as it was.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::{Pool, Sqlite};

use super::carry;
use super::metadata::{self, KenneyLicense, Lookup, MetadataStep};
use super::place::{place, PlaceStep};
use super::records::{self, InsertedRows, ManifestRow, Output, Recipe};
use super::unity_glb::{self, Conversion};
use super::unitypackage::{extract_unitypackage, extract_variant, ExtractedFile, SkippedEntry};
use super::unreal_glb;
use super::zipfile::extract_zip;
use crate::db::{Database, LibraryHandle};
use crate::fs_swap;
use crate::watcher::{self, IndexLock, Prune};

/// What Cancel answers once a build holds the lock.
pub const FINISHING: &str = "Finishing, can't cancel now";
const CHANGED: &str = "The pack changed during the build; Rebuild again";

/// `fs_swap` tags of the folders beside the pack.
pub(super) const STAGING: &str = "import";
pub(super) const PREV: &str = "prev";
pub(super) const FAILED: &str = "failed";

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

/// Where a test can pause a build, make it fail or crash it (a panic, which leaves the disk and
/// the records as a killed process would).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// Staged, its manifest rows written, before step 1.
    Staged,
    /// Step 3's journal written, before step 4.
    Journaled,
    /// Step 4's rename of the live folder aside (failure only).
    MoveAside,
    /// Step 4, between its renames (crash only).
    Aside,
    /// Step 4's rename of the staging folder to the live name (failure only).
    MoveIn,
    /// Step 4 failed with nothing of the build in place, before its journal is cleared.
    Unjournal,
    /// Step 5's carry, between its aside and place passes.
    CarryAside,
    /// Step 5, after the index.
    Indexed,
    /// Step 6 committed, before step 7.
    Committed,
    /// A rollback, between its renames (crash only).
    RollbackAside,
    /// A rollback, after its renames (crash only).
    RollbackRenamed,
    /// A rollback, after its step 4 transaction and before it deletes `.failed-<build id>`.
    RollbackCleared,
    /// The engine's claim of a queued run, before its `UPDATE`.
    Claim,
    /// A failed claim, after the engine cleared the active run and before it checks for Cancel.
    ClaimFailed,
    /// `Engine::cancel_run`, after it found the run isn't the active one.
    CancelLookup,
}

/// Test hooks, as autotag undo's `Crashes`. They do nothing outside tests.
#[derive(Default)]
pub struct Hooks {
    #[cfg(test)]
    points: Mutex<Vec<(Point, Hook)>>,
    /// The store page lookup's HEAD request, injected.
    #[cfg(test)]
    head: Mutex<Option<metadata::Head>>,
}

#[cfg(test)]
#[derive(Clone)]
enum Hook {
    Pause(Arc<Pause>),
    Fail,
    /// Fails the first time only.
    FailOnce,
    /// Panics the first time only, as a killed process stops: nothing after it runs.
    Crash,
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

    /// Answers the store page lookup's HEAD requests with `head`.
    #[cfg(test)]
    pub fn store_pages(&self, head: metadata::Head) {
        *self.head.lock().unwrap() = Some(head);
    }

    /// The store page lookup's HEAD request. In tests, the injected one, else one that never
    /// reaches the network.
    pub(crate) fn head(&self) -> metadata::Head {
        #[cfg(test)]
        {
            use futures_util::FutureExt;
            let injected = self.head.lock().unwrap().clone();
            injected.unwrap_or_else(|| {
                Arc::new(|url: String| {
                    async move { Err(format!("no network in tests: {url}")) }.boxed()
                })
            })
        }
        #[cfg(not(test))]
        metadata::http_head()
    }

    #[cfg(test)]
    pub fn fail(&self, point: Point) {
        self.points.lock().unwrap().push((point, Hook::Fail));
    }

    #[cfg(test)]
    pub fn fail_once(&self, point: Point) {
        self.points.lock().unwrap().push((point, Hook::FailOnce));
    }

    #[cfg(test)]
    pub fn crash(&self, point: Point) {
        self.points.lock().unwrap().push((point, Hook::Crash));
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

    pub(crate) fn fails(&self, _point: Point) -> bool {
        #[cfg(test)]
        {
            let mut points = self.points.lock().unwrap();
            let once = points
                .iter()
                .position(|(p, h)| *p == _point && matches!(h, Hook::FailOnce));
            if let Some(once) = once {
                points.remove(once);
                return true;
            }
            if points
                .iter()
                .any(|(p, h)| *p == _point && matches!(h, Hook::Fail))
            {
                return true;
            }
        }
        false
    }

    /// Panics at `point` if a test asked to, once, so the test's next build runs through.
    pub(crate) fn crashes(&self, _point: Point) {
        #[cfg(test)]
        {
            let mut points = self.points.lock().unwrap();
            let crash = points
                .iter()
                .position(|(p, h)| *p == _point && matches!(h, Hook::Crash));
            if let Some(crash) = crash {
                points.remove(crash);
                drop(points);
                panic!("injected crash at {_point:?}");
            }
        }
    }

    /// Pauses at `point` if a test asked to, then panics if it asked to.
    pub(crate) async fn stop(&self, point: Point) {
        #[cfg(test)]
        for hook in self.hooks(point) {
            if let Hook::Pause(pause) = hook {
                pause.reached.notify_one();
                pause.release.notified().await;
            }
        }
        self.crashes(point);
    }

    /// Pauses at `point` if a test asked to, then panics or fails if it asked to.
    pub(crate) async fn at(&self, point: Point) -> Result<(), String> {
        self.stop(point).await;
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
    /// What the convert phase runs with.
    pub tools: unity_glb::Tools,
    /// What `export_unreal` runs with.
    pub unreal: unreal_glb::Tools,
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
    extract: Extract,
    place: Vec<PlaceStep>,
    /// The `unity_glb` step's options, when it is on.
    convert: Option<unity_glb::Options>,
    metadata: Option<MetadataStep>,
}

/// The steps of a recipe, by phase.
#[derive(Default)]
struct Parts {
    extracts: Vec<Extract>,
    place: Vec<PlaceStep>,
    converts: Vec<unity_glb::Options>,
    metadata: Vec<MetadataStep>,
}

/// The extract step, by the original's kind.
#[derive(Clone)]
enum Extract {
    /// The original `.unitypackage`, or with a `variant`, the one at that path in a zip original.
    Unitypackage {
        variant: Option<String>,
        subpath: Option<String>,
    },
    /// A zip original, or one folder of it.
    Zip { subpath: Option<String> },
    /// A zip original's Unreal project, exported to GLBs.
    Unreal(unreal_glb::Options),
}

/// Refuses steps a build of an original of `kind` couldn't run: an unknown step, invalid
/// options, other than exactly one enabled extract step, or more than one enabled `unity_glb`.
pub fn check_steps(kind: &str, steps: &[records::Step]) -> Result<(), String> {
    for step in steps {
        Plan::parts(kind, &[(step.kind.clone(), step.options.clone())])?;
    }
    let enabled: Vec<(String, serde_json::Value)> = steps
        .iter()
        .filter(|s| s.enabled)
        .map(|s| (s.kind.clone(), s.options.clone()))
        .collect();
    Plan::checked(Plan::parts(kind, &enabled)?)?;
    Ok(())
}

impl Plan {
    fn of(recipe: &Recipe) -> Result<Self, String> {
        Self::checked(Self::parts(&recipe.kind, &recipe.steps)?)
    }

    fn checked(parts: Parts) -> Result<Self, String> {
        let Ok([extract]) = <[Extract; 1]>::try_from(parts.extracts) else {
            return Err("A pack needs exactly one extract step".into());
        };
        if parts.converts.len() > 1 {
            return Err("A pack runs at most one unity_glb step".into());
        }
        if parts.metadata.len() > 1 {
            return Err("A pack has at most one metadata step".into());
        }
        Ok(Plan {
            extract,
            place: parts.place,
            convert: parts.converts.into_iter().next(),
            metadata: parts.metadata.into_iter().next(),
        })
    }

    /// The steps among `steps` by phase, for an original of `kind`, refusing an unknown step or
    /// invalid options.
    fn parts(kind: &str, steps: &[(String, serde_json::Value)]) -> Result<Parts, String> {
        let zip = match kind {
            "unitypackage" => false,
            "zip" => true,
            other => return Err(format!("{other} originals can't be built yet")),
        };
        let mut parts = Parts::default();
        let (extracts, place) = (&mut parts.extracts, &mut parts.place);
        for (kind, options) in steps {
            let text = |key: &str| options[key].as_str().map(str::to_string);
            match kind.as_str() {
                "extract_unitypackage" => {
                    let variant = text("variant");
                    if zip != variant.is_some() {
                        return Err(if zip {
                            "The extract_unitypackage step of a zip names its variant".into()
                        } else {
                            "A .unitypackage original has no variant".into()
                        });
                    }
                    extracts.push(Extract::Unitypackage {
                        variant,
                        subpath: text("subpath"),
                    });
                }
                "extract_zip" if zip => extracts.push(Extract::Zip {
                    subpath: text("subpath"),
                }),
                "extract_zip" => return Err("The extract_zip step needs a zip original".into()),
                "export_unreal" if zip => {
                    extracts.push(Extract::Unreal(unreal_glb::Options::parse(options)?))
                }
                "export_unreal" => return Err("The export_unreal step needs a zip original".into()),
                "flatten" | "hoist" | "rename" | "exclude" => {
                    let mut step = options.clone();
                    step["kind"] = kind.clone().into();
                    place.push(
                        serde_json::from_value(step)
                            .map_err(|e| format!("The {kind} step's options are invalid: {e}"))?,
                    );
                }
                "metadata" => parts.metadata.push(
                    serde_json::from_value(options.clone())
                        .map_err(|e| format!("The metadata step's options are invalid: {e}"))?,
                ),
                "unity_glb" => parts.converts.push(unity_glb::Options::parse(options)?),
                other => return Err(format!("The {other} step can't run yet")),
            }
        }
        Ok(parts)
    }
}

/// Builds pack `pack_id`, queued at `queued_rev`: its first build, or a rebuild over the
/// committed one. Fills `warnings` with the entries the extraction skipped.
pub async fn build(
    env: &BuildEnv<'_>,
    pack_id: &str,
    queued_rev: i64,
    warnings: &mut Vec<SkippedEntry>,
) -> Result<(), Stop> {
    let pool = &env.library.pool;
    let recipe = records::recipe(pool, pack_id).await.map_err(db_error)?;
    if recipe.swap_state.is_some() || recipe.recipe_rev != queued_rev {
        return Err(Stop::Failed(CHANGED.to_string()));
    }
    let plan = Plan::of(&recipe)?;
    let root = env.library.root_path.clone();
    let live = root.join(&recipe.target);
    let build_id = uuid::Uuid::new_v4().to_string();
    if !plainly_inside(&root, &live) {
        return Err(Stop::Failed(format!(
            "A folder on the way to {} is a link: a pack is built only inside the library",
            recipe.target
        )));
    }
    let previous = match &recipe.current_build {
        Some(build) => records::manifest(pool, pack_id, build)
            .await
            .map_err(db_error)?,
        None => Vec::new(),
    };
    if let Some(changed) = live_changed(&live, &recipe, &previous).await {
        return Err(Stop::Failed(match recipe.current_build {
            None => format!("{} already exists in the library", recipe.target),
            Some(_) => format!("{changed} changed outside OpenDAM since the pack's last build"),
        }));
    }
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

    let outputs = match stage(
        env, &recipe, &plan, &staging, &build_id, &previous, warnings,
    )
    .await
    {
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
    let (index_guard, advisory) = match both_locks(env, &index_lock, &root).await {
        Ok(locks) => locks,
        Err(stop) => return Err(cleanup.run(stop).await),
    };
    // Every holder of the advisory lock lets go through `release`, which runs a structure pass
    // left pending meanwhile, still under `index_lock`.
    let swapped = swap_in(
        env,
        &recipe,
        queued_rev,
        &previous,
        &staging,
        &build_id,
        &outputs,
        &cleanup,
        &index_guard,
        &advisory,
    )
    .await;
    advisory.release(pool, &root).await;
    drop(index_guard);
    swapped
}

/// Steps 2-7, or the rollback, under both locks, with Cancel past.
#[allow(clippy::too_many_arguments)]
async fn swap_in(
    env: &BuildEnv<'_>,
    recipe: &Recipe,
    queued_rev: i64,
    previous: &[ManifestRow],
    staging: &Path,
    build_id: &str,
    outputs: &[Output],
    cleanup: &Cleanup<'_>,
    index_guard: &tokio::sync::MutexGuard<'_, ()>,
    advisory: &IndexLock,
) -> Result<(), Stop> {
    let pool = &env.library.pool;
    let pack_id = recipe.pack_id.as_str();
    let live = env.library.root_path.join(&recipe.target);
    let created = cleanup.created;
    if !env.control.lock() {
        return Err(cleanup.run(Stop::Cancelled).await);
    }
    // Past Cancel: the Imports view's Cancel reads "Finishing, can't cancel now" from here on.
    (env.progress)(Progress {
        step: "re-checking",
        done: 0,
        total: 0,
    });

    // ── 2. Re-check ─────────────────────────────────────────────────────
    if !plainly_inside(&env.library.root_path, &live) {
        return Err(cleanup.run(Stop::Failed(CHANGED.to_string())).await);
    }
    let checked = recheck(
        pool, recipe, queued_rev, previous, &live, staging, build_id, outputs,
    )
    .await;
    if let Err(stop) = checked {
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
    .bind(build_id)
    .bind(swap_prev)
    .bind(pack_id)
    .execute(pool)
    .await;
    if !matches!(&journaled, Ok(done) if done.rows_affected() == 1) {
        return Err(cleanup.run(CHANGED.to_string().into()).await);
    }
    env.hooks.stop(Point::Journaled).await;

    // ── 4. Swap ─────────────────────────────────────────────────────────
    (env.progress)(Progress {
        step: "swapping",
        done: 0,
        total: 0,
    });
    let prev = swap_prev.then(|| fs_swap::sibling(&live, PREV, build_id));
    let swapped = fs_swap::swap(
        &live,
        staging,
        prev.as_deref(),
        fs_swap::Old::Keep,
        |a, b| {
            if a == staging && b == live {
                env.hooks.crashes(Point::Aside);
                if env.hooks.fails(Point::MoveIn) {
                    return Err(std::io::Error::other("injected failure"));
                }
            } else if a == live && env.hooks.fails(Point::MoveAside) {
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
            env.hooks.stop(Point::Unjournal).await;
            if let Err(e) = unjournal(pool, pack_id, build_id).await {
                return Err(Stop::Failed(format!(
                    "{failed}. Clearing its journal failed: {e}"
                )));
            }
            return Err(cleanup.run(failed.into()).await);
        }
    }

    // ── 5. Carry, then index without pruning, 6. Commit ─────────────────
    (env.progress)(Progress {
        step: "indexing",
        done: 0,
        total: 0,
    });
    let pack_path = [live.to_string_lossy().into_owned()];
    let committed = async {
        if let Some(prev_build) = &recipe.current_build {
            carry::carry(
                pool,
                pack_id,
                &recipe.target,
                prev_build,
                build_id,
                env.hooks,
            )
            .await
            .map_err(|e| format!("Carrying the moved files' rows failed: {e}"))?;
        }
        let journal = InsertedRows::after(pool, pack_id, build_id)
            .await
            .map_err(|e| e.to_string())?;
        watcher::index_paths_locked(
            env.db,
            &env.library.id,
            &pack_path,
            Prune::Defer(&journal),
            index_guard,
            advisory,
        )
        .await
        .map_err(|e| format!("Indexing the pack failed: {e:#}"))?;
        env.hooks.at(Point::Indexed).await?;
        sqlx::query(
            "UPDATE import_packs SET current_build = ?, status = 'built', built_at = ?,
             swap_state = NULL, swap_build = NULL, swap_prev = NULL WHERE id = ?",
        )
        .bind(build_id)
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
            target: &recipe.target,
            prev: prev.as_deref(),
            build_id,
            created,
        };
        return Err(rollback(env, index_guard, advisory, pack_id, &swap, error).await);
    }
    env.hooks.stop(Point::Committed).await;

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
        index_guard,
        advisory,
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
            .bind(build_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM import_moves WHERE pack_id = ? AND build_id = ?")
            .bind(pack_id)
            .bind(build_id)
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

/// Whether the pack folder `live` stays where it says, inside the library at `root`: no folder
/// between them that exists is a symlink, so staging, the swap and a rollback never reach
/// outside the library or into another folder of it.
pub(crate) fn plainly_inside(root: &Path, live: &Path) -> bool {
    let (Ok(root_real), Ok(rel)) = (root.canonicalize(), live.strip_prefix(root)) else {
        return false;
    };
    live.ancestors()
        .find(|dir| dir.symlink_metadata().is_ok())
        .and_then(|dir| Some((dir.canonicalize().ok()?, dir.strip_prefix(root).ok()?)))
        .is_some_and(|(real, below)| real == root_real.join(below) && rel.starts_with(below))
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
    previous: &[ManifestRow],
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
    let extraction = match &plan.extract {
        Extract::Unreal(options) => {
            unreal_glb::Export {
                original: &original,
                original_sha256: &recipe.original_sha256,
                staging,
                options,
                tools: &env.unreal,
                control: env.control.clone(),
                progress: env.progress.clone(),
            }
            .run()
            .await?
        }
        extract => {
            let (to, extract) = (staging.to_path_buf(), extract.clone());
            blocking(move || {
                match &extract {
                    Extract::Unitypackage {
                        variant: None,
                        subpath,
                    } => extract_unitypackage(&original, &to, subpath.as_deref()),
                    Extract::Unitypackage {
                        variant: Some(variant),
                        subpath,
                    } => extract_variant(&original, variant, &to, subpath.as_deref()),
                    Extract::Zip { subpath } => extract_zip(&original, &to, subpath.as_deref()),
                    Extract::Unreal(_) => unreachable!("exported above"),
                }
                .map_err(|e| format!("{e:#}"))
            })
            .await?
        }
    };
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

    // A staged file the previous build had, by file key, size and sha256, keeps its mtime.
    let unchanged: Arc<Unchanged> = Arc::new(
        previous
            .iter()
            .filter_map(|row| {
                let key = (row.source_path.clone()?, row.generated);
                Some((key, (row.size, row.sha256.clone(), row.mtime_ns)))
            })
            .collect(),
    );
    // The phases after placing, in plan order: convert, then metadata, which sees the GLBs.
    // Files are hashed once at the end, except that a conversion needs its inputs' hashes first.
    let prefix = format!("{}/", recipe.target);
    let mut pending: Vec<(ExtractedFile, bool)> = files.into_iter().map(|f| (f, false)).collect();
    let mut outputs = Vec::new();
    if let Some(options) = &plan.convert {
        outputs = hash_staged(
            env,
            recipe,
            staging,
            std::mem::take(&mut pending),
            &unchanged,
        )
        .await?;
        cancelled()?;
        let files: Vec<unity_glb::Staged> = outputs
            .iter()
            .map(|o| unity_glb::Staged {
                path: o.path[prefix.len()..].to_string(),
                source_path: o.source_path.clone(),
                sha256: o.sha256.clone(),
            })
            .collect();
        let models = Conversion {
            staging,
            files: &files,
            original_sha256: &recipe.original_sha256,
            options,
            tools: &env.tools,
            control: env.control.clone(),
            progress: env.progress.clone(),
        }
        .run()
        .await?;
        let generated = models
            .into_iter()
            .map(|m| {
                // Its size is read from disk when it is hashed, below.
                let glb = ExtractedFile {
                    path: m.glb,
                    source_path: m.source_path,
                    size: 0,
                };
                (glb, true)
            })
            .collect();
        outputs.extend(hash_staged(env, recipe, staging, generated, &unchanged).await?);
        cancelled()?;
    }
    if let Some(step) = &plan.metadata {
        let files: Vec<ExtractedFile> = outputs
            .iter()
            .map(|o| ExtractedFile {
                path: o.path[prefix.len()..].to_string(),
                source_path: o.source_path.clone(),
                size: o.size,
            })
            .chain(pending.iter().map(|(f, _)| f.clone()))
            .collect();
        let package = write_package(env, recipe, step, staging, &files).await?;
        pending.push((package, true));
        cancelled()?;
    }
    outputs.extend(hash_staged(env, recipe, staging, pending, &unchanged).await?);
    cancelled()?;
    outputs.sort_by(|a, b| a.path.cmp(&b.path));

    records::write_outputs(&env.library.pool, &recipe.pack_id, build_id, &outputs)
        .await
        .map_err(db_error)?;
    env.hooks.at(Point::Staged).await?;
    cancelled()?;
    Ok(outputs)
}

/// The previous build's rows by file key (source path, generated): size, sha256 and mtime.
type Unchanged = HashMap<(String, bool), (i64, String, i64)>;

/// Hashes the staged files `files`, each with whether it is generated, as their manifest rows
/// (a file's size is read from disk). A file the previous build had, by file key, size and sha256, gets that
/// build's mtime, so the index sees it unchanged.
async fn hash_staged(
    env: &BuildEnv<'_>,
    recipe: &Recipe,
    staging: &Path,
    files: Vec<(ExtractedFile, bool)>,
    unchanged: &Arc<Unchanged>,
) -> Result<Vec<Output>, Stop> {
    let (to, target, control, progress, unchanged) = (
        staging.to_path_buf(),
        recipe.target.clone(),
        env.control.clone(),
        env.progress.clone(),
        unchanged.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let total = files.len() as u64;
        let mut outputs = Vec::with_capacity(files.len());
        for (done, (file, generated)) in files.into_iter().enumerate() {
            let (rel, source_path) = (file.path, file.source_path);
            if control.cancelled() {
                return Err(Stop::Cancelled);
            }
            progress(Progress {
                step: "hashing",
                done: done as u64,
                total,
            });
            let path = to.join(&rel);
            let read = |e: std::io::Error| format!("The staged {rel} can't be read: {e}");
            let (size, mut mtime_ns) = file_stamp(&path).map_err(read)?;
            let sha256 = crate::export::hashes::sha256_file(&path).map_err(read)?;
            let key = (records::source_json(&source_path), generated);
            if let Some((_, _, before)) = unchanged
                .get(&key)
                .filter(|(s, sha, _)| *s == size as i64 && *sha == sha256)
            {
                if *before != mtime_ns {
                    set_mtime(&path, *before)
                        .map_err(|e| format!("The staged {rel} can't be stamped: {e}"))?;
                    mtime_ns = *before;
                }
            }
            outputs.push(Output {
                path: format!("{target}/{rel}"),
                size,
                mtime_ns,
                sha256,
                source_path,
                generated,
            });
        }
        Ok(outputs)
    })
    .await
    .map_err(|e| Stop::Failed(format!("The build's worker stopped: {e}")))?
}

/// The metadata step: writes the pack's `datapackage.json` into `staging`, beside the placed
/// `files`, and returns it as a staged file, generated from no entry of the original.
async fn write_package(
    env: &BuildEnv<'_>,
    recipe: &Recipe,
    step: &MetadataStep,
    staging: &Path,
    files: &[ExtractedFile],
) -> Result<ExtractedFile, Stop> {
    (env.progress)(Progress {
        step: "writing metadata",
        done: 0,
        total: 0,
    });
    if let Some(path) = metadata::clash(files) {
        return Err(Stop::Failed(metadata::clash_message(path)));
    }
    let kenney = match metadata::license_file(files) {
        Some(file) if step.preset == metadata::Preset::Kenney => {
            let text = tokio::fs::read(staging.join(file))
                .await
                .map_err(|e| format!("The staged {file} can't be read: {e}"))?;
            Some(KenneyLicense {
                file: file.to_string(),
                header: metadata::header(&String::from_utf8_lossy(&text)),
            })
        }
        _ => None,
    };
    let folder = recipe.target.rsplit('/').next().unwrap_or(&recipe.target);
    let page = if step.looks_up() {
        let title = step.title(folder, kenney.as_ref());
        store_page(env, recipe, &[&title, folder]).await?
    } else {
        None
    };
    let package = step.package(&recipe.target, files, kenney.as_ref(), page.as_deref());
    let text = package.render();
    tokio::fs::write(staging.join(metadata::FILE), &text)
        .await
        .map_err(|e| format!("Could not write {}: {e}", metadata::FILE))?;
    Ok(ExtractedFile {
        path: metadata::FILE.to_string(),
        source_path: Vec::new(),
        size: text.len() as u64,
    })
}

/// The pack's Kenney store page, by the slugs of `names`: the answer kept in
/// `import_packs.store_page` while it is for the same URLs, else what HEAD requests find, which
/// is then kept there when they answered. A rebuild therefore makes no request unless the title
/// or the folder the slugs come from changed. The kept answer is no change to the recipe, so
/// neither this build's re-check nor another queued run fails over it (plan R24).
async fn store_page(
    env: &BuildEnv<'_>,
    recipe: &Recipe,
    names: &[&str],
) -> Result<Option<String>, Stop> {
    let urls = metadata::store_urls(names);
    if urls.is_empty() {
        return Ok(None);
    }
    let kept = recipe
        .store_page
        .as_deref()
        .and_then(|kept| serde_json::from_str::<metadata::StorePage>(kept).ok())
        .filter(|kept| kept.urls == urls);
    if let Some(kept) = kept {
        return Ok(kept.page);
    }
    (env.progress)(Progress {
        step: "looking up the store page",
        done: 0,
        total: 0,
    });
    let page = match metadata::look_up(&env.hooks.head(), &urls).await {
        Lookup::Found(url) => Some(url),
        Lookup::Miss => None,
        Lookup::Unknown => return Ok(None),
    };
    let answer = metadata::StorePage {
        urls,
        page: page.clone(),
    };
    records::set_store_page(&env.library.pool, &recipe.pack_id, &answer.to_json())
        .await
        .map_err(db_error)?;
    Ok(page)
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, Stop> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("The build's worker stopped: {e}"))?
        .map_err(Stop::Failed)
}

/// Sets the file at `path`'s mtime to `mtime_ns` nanoseconds.
fn set_mtime(path: &Path, mtime_ns: i64) -> std::io::Result<()> {
    let at = std::time::UNIX_EPOCH + Duration::from_nanos(mtime_ns.max(0) as u64);
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .set_modified(at)
}

/// A file's size and mtime in nanoseconds. An original's stamp is recorded and checked with this
/// one function, so the two always agree.
pub(crate) fn file_stamp(path: &Path) -> std::io::Result<(u64, i64)> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Ok((meta.len(), mtime.as_nanos() as i64))
}

/// Takes the library's `index_lock`, then the advisory lock, waiting up to `lock_wait` for
/// another holder of the advisory lock. Between tries it lets `index_lock` go, so this app's own
/// commands and index runs aren't held up by another machine. Cancel stops the wait. Locks taken
/// are returned even when cancelled: `RunControl::lock` decides then.
async fn both_locks<'l>(
    env: &BuildEnv<'_>,
    index_lock: &'l tokio::sync::Mutex<()>,
    root: &Path,
) -> Result<(tokio::sync::MutexGuard<'l, ()>, IndexLock), Stop> {
    let deadline = tokio::time::Instant::now() + env.lock_wait;
    loop {
        let index_guard = index_lock.lock().await;
        if let Some(lock) = IndexLock::try_acquire(root).await {
            return Ok((index_guard, lock));
        }
        drop(index_guard);
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

/// Step 2: the recipe and the committed build are as queued, the pack folder still holds the
/// current manifest's files (or still doesn't exist before a first build), and the staging
/// folder and the build's manifest rows hold exactly the files the build staged.
#[allow(clippy::too_many_arguments)]
async fn recheck(
    pool: &Pool<Sqlite>,
    queued: &Recipe,
    queued_rev: i64,
    previous: &[ManifestRow],
    live: &Path,
    staging: &Path,
    build_id: &str,
    outputs: &[Output],
) -> Result<(), Stop> {
    let now = records::recipe(pool, &queued.pack_id)
        .await
        .map_err(db_error)?;
    let same_recipe = now.recipe_rev == queued_rev
        && now.current_build == queued.current_build
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
        || live_changed(live, queued, previous).await.is_some()
        || staged.ok().as_ref() != Some(&expected)
        || rows != expected
    {
        return Err(Stop::Failed(CHANGED.to_string()));
    }
    Ok(())
}

/// What changed in the pack folder `live` since the committed build: `None` when it holds exactly
/// the files of `manifest` (that build's rows), compared by size and mtime and hashed only when
/// the mtime differs, or, before a first build, when it doesn't exist.
async fn live_changed(live: &Path, recipe: &Recipe, manifest: &[ManifestRow]) -> Option<String> {
    let folder = format!("The pack folder {}", recipe.target);
    if recipe.current_build.is_none() {
        return live.symlink_metadata().is_ok().then_some(folder);
    }
    if !live.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return Some(folder);
    }
    let (live, prefix) = (live.to_path_buf(), format!("{}/", recipe.target));
    let (manifest, stopped) = (manifest.to_vec(), folder.clone());
    tokio::task::spawn_blocking(move || {
        let Ok(files) = staged_sizes(&live) else {
            return Some(folder);
        };
        let listed: Vec<(String, i64)> = manifest
            .iter()
            .map(|row| (row.path.clone(), row.size))
            .collect();
        let found: Vec<(String, i64)> = files
            .into_iter()
            .map(|(path, size)| (format!("{prefix}{path}"), size))
            .collect();
        if found != listed {
            // The first path that differs, in either list.
            let differs = found
                .iter()
                .zip(&listed)
                .find(|(a, b)| a != b)
                .map(|(a, b)| a.0.as_str().min(b.0.as_str()).to_string());
            let extra = found.get(listed.len()).or(listed.get(found.len()));
            return differs.or(extra.map(|e| e.0.clone()));
        }
        manifest.iter().find_map(|row| {
            let path = live.join(&row.path[prefix.len()..]);
            let same = match file_stamp(&path) {
                Ok((_, mtime_ns)) if mtime_ns == row.mtime_ns => true,
                Ok(_) => crate::export::hashes::sha256_file(&path).is_ok_and(|h| h == row.sha256),
                Err(_) => false,
            };
            (!same).then(|| row.path.clone())
        })
    })
    .await
    .unwrap_or(Some(stopped))
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
pub(super) async fn unjournal(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    build_id: &str,
) -> sqlx::Result<()> {
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
    target: &'a str,
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
    env.hooks.crashes(Point::RollbackAside);
    if let Some(prev) = swap.prev {
        if let Err(e) = std::fs::rename(prev, swap.live) {
            return unfinished(format!("moving the previous folder back: {e}"));
        }
    }
    env.hooks.crashes(Point::RollbackRenamed);

    // 3-4. Rows, index, journal, then the failed build's folder.
    let rolled = Rollback {
        pool,
        pack_id,
        target: swap.target,
        build_id: swap.build_id,
        live: swap.live,
        failed: &failed,
        reindex: swap.prev.is_some(),
    };
    let refresh = Refresh::Registered {
        db: env.db,
        library: env.library,
    };
    if let Err(step) = finish_rollback(&rolled, refresh, index_guard, advisory, env.hooks).await {
        return unfinished(step);
    }
    // Only the build knows which folders it made on the way to the pack.
    remove_created(swap.created);
    Stop::Failed(error)
}

/// A swap being rolled back, its folders already in place: the failed build at `failed`, and the
/// previous folder (if any) back at `live`.
pub(super) struct Rollback<'a> {
    pub pool: &'a Pool<Sqlite>,
    pub pack_id: &'a str,
    pub target: &'a str,
    pub build_id: &'a str,
    pub live: &'a Path,
    pub failed: &'a Path,
    /// A previous folder was moved aside and is back, so the pack is reindexed. Without one the
    /// pack folder is gone again, as before the build, and an index would prune the rows just
    /// moved back.
    pub reindex: bool,
}

/// How `finish_rollback` brings the index and the structure up to date.
pub(super) enum Refresh<'a> {
    /// A build's own rollback, its library registered: `index_paths_locked`, which also stores
    /// the asset count, or without a previous folder `rebuild_library_locked`.
    Registered {
        db: &'a Database,
        library: &'a LibraryHandle,
    },
    /// Recovery, maybe before the library is registered: the lock-free index core over the
    /// library at `root`. Its caller runs the structure pass right after it.
    Core { root: &'a Path },
}

/// A rollback's steps 3 and 4 (plan Flow, Rollback), shared by a build's own rollback and swap
/// recovery, which finishes one a crash interrupted. Holding both of the library's locks, it
/// deletes the rows the build's index inserted, which frees their paths, moves the carried rows
/// back by asset id, reindexes the restored pack (`Rollback::reindex`), then clears the journal
/// and the build's records in one transaction, and only then deletes the failed build's folder.
/// On failure it returns the step that failed, the journal still set, for recovery.
pub(super) async fn finish_rollback(
    rolled: &Rollback<'_>,
    refresh: Refresh<'_>,
    index_guard: &tokio::sync::MutexGuard<'_, ()>,
    advisory: &IndexLock,
    hooks: &Hooks,
) -> Result<(), String> {
    let pool = rolled.pool;
    sqlx::query(
        "DELETE FROM assets WHERE id IN (SELECT asset_id FROM import_moves
         WHERE pack_id = ? AND build_id = ? AND kind = 'inserted')",
    )
    .bind(rolled.pack_id)
    .bind(rolled.build_id)
    .execute(pool)
    .await
    .map_err(|e| format!("deleting the rows it inserted: {e}"))?;
    carry::reverse(pool, rolled.pack_id, rolled.target, rolled.build_id)
        .await
        .map_err(|e| format!("moving the carried rows back: {e}"))?;
    match (refresh, rolled.reindex) {
        (Refresh::Registered { db, library }, true) => {
            let pack_path = [rolled.live.to_string_lossy().into_owned()];
            let indexed = watcher::index_paths_locked(
                db,
                &library.id,
                &pack_path,
                Prune::Now,
                index_guard,
                advisory,
            )
            .await;
            if let Err(e) = indexed {
                tracing::warn!("Re-indexing the restored pack: {e:#}");
            }
        }
        (Refresh::Registered { db, library }, false) => {
            crate::structure::rebuild_library_locked(db, library, index_guard, advisory).await;
        }
        (Refresh::Core { root }, true) => index_pack(pool, root, advisory, rolled.live).await,
        (Refresh::Core { .. }, false) => {}
    }
    unjournal(pool, rolled.pack_id, rolled.build_id)
        .await
        .map_err(|e| format!("clearing its journal: {e}"))?;
    hooks.stop(Point::RollbackCleared).await;
    fs_swap::discard(rolled.failed);
    Ok(())
}

/// Indexes the pack folder `live` of the library at `root` with `Prune::Now` through the
/// lock-free core, when it is a folder: walking a missing one would prune every row below it. A
/// failure is logged, as a build's settle logs it: the next refresh indexes it again.
pub(super) async fn index_pack(
    pool: &Pool<Sqlite>,
    root: &Path,
    advisory: &IndexLock,
    live: &Path,
) {
    if !live.is_dir() {
        tracing::warn!("{} is no folder now, so it isn't indexed", live.display());
        return;
    }
    let paths = [live.to_string_lossy().into_owned()];
    if let Err(e) = watcher::index_pool(pool, root, &paths, Prune::Now, Some(advisory)).await {
        tracing::warn!("Indexing the pack {}: {e:#}", live.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(kind: &str, enabled: bool, options: serde_json::Value) -> records::Step {
        records::Step {
            kind: kind.into(),
            enabled,
            options,
        }
    }

    #[test]
    fn a_pack_runs_at_most_one_unity_glb_step() {
        let extract = step("extract_unitypackage", true, json!({}));
        let convert = |enabled| step("unity_glb", enabled, json!({"max_texture": 1024}));
        assert_eq!(
            check_steps(
                "unitypackage",
                &[extract.clone(), convert(true), convert(true)]
            ),
            Err("A pack runs at most one unity_glb step".into())
        );
        assert_eq!(
            check_steps(
                "unitypackage",
                &[extract.clone(), convert(true), convert(false)]
            ),
            Ok(())
        );
        // A step that is off still needs options it could run with.
        let invalid = step("unity_glb", false, json!({"max_texture": 1}));
        assert!(check_steps("unitypackage", &[extract, invalid]).is_err());
    }
}
