//! A pass over each library that requeues the FBX/3DS previews an older preview engine made, so
//! existing libraries get textured previews without a manual Re-generate (plan
//! textured-fbx-previews C9). It requeues only while the worker process serving the queue has
//! declared, through `engine_info`, a version of at least `MODEL_PREVIEW_VERSION`. `library_meta`
//! records the version once no stale preview is left, done or waiting, so a job an older process
//! served is redone on a later run.

use crate::commands::asset::NOT_SUPPORT;
use crate::db::LibraryHandle;
use crate::preview::{claimable, enqueue, ACTIVE_JOBS};
use crate::AppState;
use anyhow::Result;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::{watch, Mutex};

/// The preview engine version whose FBX/3DS previews are current. `worker.py`'s
/// `PREVIEW_VERSION` matches it.
pub const MODEL_PREVIEW_VERSION: i64 = 2;
const META_KEY: &str = "model_preview_version";
const BATCH_SIZE: usize = 200;

/// Runs one pass at a time, so a library's candidates are never read twice at once.
static RUNS: Mutex<()> = Mutex::const_new(());

/// How a call to `run_pass` ended.
#[derive(Debug, PartialEq)]
pub(crate) enum Pass {
    /// No worker process has declared its version yet, so nothing was read.
    NoEngineYet,
    /// `library_meta` records `MODEL_PREVIEW_VERSION` or later.
    AlreadyDone,
    /// The serving worker declared an older version, so nothing was requeued.
    OldEngine,
    /// Requeued this many, and stale previews are left, done or waiting, so no key.
    Waiting(usize),
    /// Nothing stale is left, and the key is recorded.
    Recorded,
}

/// What one run over the libraries did.
#[derive(Debug, Default, PartialEq)]
struct Run {
    /// Libraries in which the run requeued previews.
    requeued_in: usize,
    /// Logged once when the serving worker is older and a library still needs the pass.
    warning: Option<String>,
}

/// Runs the pass in the background for every open library, or only `library_id`.
pub fn spawn(state: Arc<AppState>, library_id: Option<String>) {
    tauri::async_runtime::spawn(async move { run(&state, library_id.as_deref()).await });
}

/// Runs the pass for every open library now, and again each time the queue publishes the
/// engine version of a new worker process.
pub fn start(state: Arc<AppState>) {
    let published = state.engine_version.subscribe();
    tauri::async_runtime::spawn(follow_engine(published, move || {
        let state = state.clone();
        async move { run(&state, None).await }
    }));
}

/// Calls `pass` once, then after each change of the published engine version, until the
/// sender is gone. Subscribing before the first call means no publish is missed.
async fn follow_engine<F: Future<Output = ()>>(
    mut published: watch::Receiver<Option<i64>>,
    mut pass: impl FnMut() -> F,
) {
    loop {
        published.borrow_and_update();
        pass().await;
        if published.changed().await.is_err() {
            return;
        }
    }
}

async fn run(state: &AppState, library_id: Option<&str>) {
    let _one_at_a_time = RUNS.lock().await;
    let version = *state.engine_version.borrow();
    let worker = state.preview_worker.get().map(|p| p.as_path());
    let libs = state.db.get_all_libraries().await;
    let index_lock = |id: &str| state.db.index_lock(id);
    let run = run_libraries(&libs, library_id, version, worker, index_lock).await;
    if let Some(warning) = run.warning {
        tracing::warn!("{warning}");
    }
    if run.requeued_in > 0 {
        let _ = state.app_handle.emit("library:updated", ());
    }
}

/// Runs the pass over each of `libs`, or only `library_id`, under the published engine
/// `version` of the `worker` serving the queue. `index_lock` gives a library's
/// `Database::index_lock`.
async fn run_libraries(
    libs: &[LibraryHandle],
    library_id: Option<&str>,
    version: Option<i64>,
    worker: Option<&Path>,
    index_lock: impl Fn(&str) -> Arc<Mutex<()>>,
) -> Run {
    let mut run = Run::default();
    let mut old_engine = false;
    for lib in libs {
        if library_id.is_some_and(|id| id != lib.id) {
            continue;
        }
        match run_pass(lib, version, BATCH_SIZE, &index_lock(&lib.id)).await {
            Ok(Pass::Waiting(n)) if n > 0 => run.requeued_in += 1,
            Ok(Pass::OldEngine) => old_engine = true,
            Ok(_) => {}
            Err(e) => tracing::error!("Model preview pass of library {} failed: {}", lib.id, e),
        }
    }
    if old_engine {
        run.warning = Some(format!(
            "The preview worker {} declares engine version {}, older than {}, so existing FBX \
             and 3DS previews are not regenerated. Update the installed engine (make install) \
             or point OPENDAM_WORKER at a current worker.py.",
            worker.map_or("(not found)".into(), |w| w.display().to_string()),
            version.unwrap_or_default(),
            MODEL_PREVIEW_VERSION
        ));
    }
    run
}

/// An FBX/3DS item the queue would claim (`preview::claimable`), never a support file, whose
/// preview is from an engine older than `MODEL_PREVIEW_VERSION`. An old engine's result has no
/// `preview_version`, which reads as 1.
fn stale() -> String {
    format!(
        "lower(a.extension) IN ('fbx', '3ds') AND {NOT_SUPPORT} AND {}
         AND coalesce(a.preview_version, 1) < {MODEL_PREVIEW_VERSION}",
        claimable()
    )
}

fn active_job() -> String {
    format!(
        "EXISTS (SELECT 1 FROM preview_jobs p WHERE p.asset_id = a.id AND p.status IN {ACTIVE_JOBS})"
    )
}

/// Runs the pass over `lib` under the published engine `version`: requeues each stale `done`
/// preview with no active job, `batch_size` to a transaction in rowid order, then records the
/// key only when no stale preview is left, done or waiting. Errored previews are left alone.
///
/// Past the gate it holds the library's `index_lock`, so no structure pass commits between
/// reading the candidates and requeueing them: one could make a candidate a group member or
/// pictured, which the queue never claims, and only the structure pass takes those off it.
pub(crate) async fn run_pass(
    lib: &LibraryHandle,
    version: Option<i64>,
    batch_size: usize,
    index_lock: &Mutex<()>,
) -> Result<Pass> {
    let Some(version) = version else {
        return Ok(Pass::NoEngineYet);
    };
    let recorded: Option<String> =
        sqlx::query_scalar("SELECT value FROM library_meta WHERE key = ?")
            .bind(META_KEY)
            .fetch_optional(&lib.pool)
            .await?;
    if recorded.and_then(|v| v.parse::<i64>().ok()) >= Some(MODEL_PREVIEW_VERSION) {
        return Ok(Pass::AlreadyDone);
    }
    if version < MODEL_PREVIEW_VERSION {
        return Ok(Pass::OldEngine);
    }
    let _index_guard = index_lock.lock().await;

    let candidates: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT a.id FROM assets a
         WHERE {} AND a.preview_status = 'done' AND NOT {}
         ORDER BY a.rowid",
        stale(),
        active_job()
    ))
    .fetch_all(&lib.pool)
    .await?;
    let now = chrono::Utc::now().timestamp_millis();
    for batch in candidates.chunks(batch_size) {
        let mut tx = lib.pool.begin().await?;
        for id in batch {
            enqueue(&mut tx, id, now).await?;
        }
        tx.commit().await?;
    }

    let left: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM assets a
                        WHERE {} AND (a.preview_status = 'done' OR {}))",
        stale(),
        active_job()
    ))
    .fetch_one(&lib.pool)
    .await?;
    tracing::info!(
        "Model preview pass of library {}: {} requeued{}",
        lib.id,
        candidates.len(),
        if left { "" } else { ", none left" }
    );
    if left {
        return Ok(Pass::Waiting(candidates.len()));
    }
    sqlx::query(
        "INSERT INTO library_meta (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(META_KEY)
    .bind(MODEL_PREVIEW_VERSION.to_string())
    .execute(&lib.pool)
    .await?;
    Ok(Pass::Recorded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::{complete_job, EngineReport};
    use serde_json::json;
    use sqlx::{Pool, Sqlite};

    /// One `assets` row: id, extension, group, preview status and version.
    type Row<'a> = (&'a str, &'a str, Option<&'a str>, &'a str, Option<i64>);

    async fn add_asset(pool: &Pool<Sqlite>, (id, ext, group, status, version): Row<'_>) {
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, group_id, preview_status, preview_version)
             VALUES (?, ?, ?, ?, '3d_model', 1, 0, 0, ?, ?, ?)",
        )
        .bind(id)
        .bind(format!("{id}/a.{ext}"))
        .bind(format!("a.{ext}"))
        .bind(ext)
        .bind(group)
        .bind(status)
        .bind(version)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn add_job(pool: &Pool<Sqlite>, asset: &str, status: &str) {
        sqlx::query(
            "INSERT INTO preview_jobs (id, asset_id, status, created_at) VALUES (?, ?, ?, 0)",
        )
        .bind(format!("old-{asset}"))
        .bind(asset)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn library(id: &str, rows: &[Row<'_>]) -> LibraryHandle {
        let pool = crate::db::memory_library_pool().await;
        for &row in rows {
            add_asset(&pool, row).await;
        }
        LibraryHandle {
            id: id.into(),
            name: id.into(),
            root_path: std::env::temp_dir().join(format!("opendam-regen-{id}")),
            pool,
        }
    }

    /// The assets with a waiting job, and every asset's (id, preview status), each sorted.
    async fn queue(lib: &LibraryHandle) -> (Vec<String>, Vec<(String, String)>) {
        let waiting = sqlx::query_scalar(
            "SELECT asset_id FROM preview_jobs WHERE status = 'pending' ORDER BY asset_id",
        )
        .fetch_all(&lib.pool)
        .await
        .unwrap();
        let statuses = sqlx::query_as("SELECT id, preview_status FROM assets ORDER BY id")
            .fetch_all(&lib.pool)
            .await
            .unwrap();
        (waiting, statuses)
    }

    async fn key(lib: &LibraryHandle) -> Option<String> {
        sqlx::query_scalar("SELECT value FROM library_meta WHERE key = 'model_preview_version'")
            .fetch_optional(&lib.pool)
            .await
            .unwrap()
    }

    /// Completes each waiting job as an engine of `version` would, `None` being an old engine
    /// that reports none.
    async fn complete_waiting(lib: &LibraryHandle, version: Option<i64>) {
        let jobs: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, asset_id FROM preview_jobs WHERE status = 'pending' ORDER BY rowid",
        )
        .fetch_all(&lib.pool)
        .await
        .unwrap();
        let report = EngineReport::of_result(&json!({"ok": true, "preview_version": version}));
        for (job, asset) in jobs {
            complete_job(&lib.pool, &lib.root_path, &job, &asset, None, None, &report).await;
        }
    }

    /// A fresh index lock for each library, which nothing else holds.
    fn no_lock(_: &str) -> Arc<Mutex<()>> {
        Arc::default()
    }

    fn strings(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn requeues_stale_fbx_and_3ds_items_and_a_stand_in_and_nothing_else() {
        let lib = library(
            "lib",
            &[
                ("fbx", "fbx", None, "done", None),
                ("tds", "3ds", None, "done", Some(1)),
                ("upper", "FBX", None, "done", None),
                // A group led by a GLB: its FBX member is never previewed
                ("glb", "glb", Some("glb"), "done", None),
                ("member", "fbx", Some("glb"), "done", None),
                // A group led by a .blend, which takes no preview: its FBX stands in
                ("blend", "blend", Some("blend"), "none", None),
                ("stand", "fbx", Some("blend"), "done", None),
                ("pictured", "fbx", None, "done", None),
                ("error", "fbx", None, "error", None),
                ("obj", "obj", None, "done", None),
                ("current", "fbx", None, "done", Some(2)),
                ("running", "fbx", None, "pending", None),
                // A done preview whose re-generate already waits on the queue
                ("rerun", "fbx", None, "done", None),
            ],
        )
        .await;
        add_job(&lib.pool, "running", "processing").await;
        add_job(&lib.pool, "rerun", "fallback").await;
        // `pictured` shows a render that fits a card, a support file
        sqlx::query("UPDATE assets SET bundled_preview = 'Previews/p.png' WHERE id = 'pictured'")
            .execute(&lib.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, support_role, preview_status, render_width, render_height)
             VALUES ('render', 'Previews/p.png', 'p.png', 'png', 'texture', 1, 0, 0, 'model_preview', 'done', 512, 512)",
        )
        .execute(&lib.pool)
        .await
        .unwrap();
        let before = queue(&lib).await.1;

        // Batches of 2
        assert_eq!(
            run_pass(&lib, Some(2), 2, &Mutex::new(())).await.unwrap(),
            Pass::Waiting(4)
        );

        let (waiting, statuses) = queue(&lib).await;
        let requeued = ["fbx", "stand", "tds", "upper"];
        assert_eq!(waiting, strings(&requeued));
        for ((id, status), (_, old)) in statuses.iter().zip(&before) {
            let expected = if requeued.contains(&id.as_str()) {
                "pending"
            } else {
                old
            };
            assert_eq!(status, expected, "{id}");
        }
        // `running` and `rerun` keep their one job each, and the key waits for every job
        let jobs: Vec<(String, i64)> = sqlx::query_as(
            "SELECT asset_id, count(*) FROM preview_jobs
             WHERE asset_id IN ('running', 'rerun') GROUP BY asset_id ORDER BY asset_id",
        )
        .fetch_all(&lib.pool)
        .await
        .unwrap();
        assert_eq!(jobs, [("rerun".into(), 1), ("running".into(), 1)]);
        assert_eq!(key(&lib).await, None);
    }

    #[tokio::test]
    async fn records_the_key_only_once_the_requeued_jobs_complete_at_the_current_version() {
        let lib = library(
            "lib",
            &[
                ("a", "fbx", None, "done", None),
                ("b", "3ds", None, "done", Some(1)),
            ],
        )
        .await;

        assert_eq!(
            run_pass(&lib, Some(2), 200, &Mutex::new(())).await.unwrap(),
            Pass::Waiting(2)
        );
        assert_eq!(key(&lib).await, None);
        // The jobs still wait: nothing more is queued and nothing recorded
        assert_eq!(
            run_pass(&lib, Some(2), 200, &Mutex::new(())).await.unwrap(),
            Pass::Waiting(0)
        );
        assert_eq!(key(&lib).await, None);

        // An older process served them: they are stale again
        complete_waiting(&lib, None).await;
        assert_eq!(
            run_pass(&lib, Some(2), 200, &Mutex::new(())).await.unwrap(),
            Pass::Waiting(2)
        );
        assert_eq!(queue(&lib).await.0, strings(&["a", "b"]));
        assert_eq!(key(&lib).await, None);

        complete_waiting(&lib, Some(2)).await;
        assert_eq!(
            run_pass(&lib, Some(2), 200, &Mutex::new(())).await.unwrap(),
            Pass::Recorded
        );
        assert_eq!(key(&lib).await.as_deref(), Some("2"));
        assert_eq!(
            run_pass(&lib, Some(2), 200, &Mutex::new(())).await.unwrap(),
            Pass::AlreadyDone
        );
    }

    #[tokio::test]
    async fn waits_silently_for_an_engine_and_warns_about_an_old_one() {
        let lib = library("lib", &[("a", "fbx", None, "done", None)]).await;
        let libs = std::slice::from_ref(&lib);
        let worker = Path::new("/opt/engine/worker.py");
        let untouched = queue(&lib).await;

        // None yet: no requeue, no key, no warning
        assert_eq!(
            run_libraries(libs, None, None, Some(worker), no_lock).await,
            Run::default()
        );
        assert_eq!(
            run_pass(&lib, None, 200, &Mutex::new(())).await.unwrap(),
            Pass::NoEngineYet
        );
        assert_eq!(queue(&lib).await, untouched);
        assert_eq!(key(&lib).await, None);

        // An older engine: no requeue, no key, one warning naming the worker
        let run = run_libraries(libs, None, Some(1), Some(worker), no_lock).await;
        assert_eq!(run.requeued_in, 0);
        let warning = run.warning.expect("a warning");
        assert!(warning.contains("/opt/engine/worker.py"), "{warning}");
        assert_eq!(queue(&lib).await, untouched);
        assert_eq!(key(&lib).await, None);

        // A library already recorded needs no pass, so an old engine draws no warning there
        sqlx::query("INSERT INTO library_meta (key, value) VALUES ('model_preview_version', '2')")
            .execute(&lib.pool)
            .await
            .unwrap();
        assert_eq!(
            run_libraries(libs, None, Some(1), Some(worker), no_lock).await,
            Run::default()
        );
    }

    #[tokio::test]
    async fn requeues_only_once_the_librarys_index_run_lets_go() {
        let lib = library("lib", &[("a", "fbx", None, "done", None)]).await;
        let index_lock = Mutex::new(());
        let index_run = index_lock.lock().await;

        let pass = run_pass(&lib, Some(2), 200, &index_lock);
        tokio::pin!(pass);
        let waited = tokio::time::timeout(std::time::Duration::from_millis(50), &mut pass).await;
        assert!(waited.is_err(), "the pass ran under an index run");
        assert!(queue(&lib).await.0.is_empty());

        drop(index_run);
        assert_eq!(pass.await.unwrap(), Pass::Waiting(1));
        assert_eq!(queue(&lib).await.0, strings(&["a"]));
    }

    #[tokio::test]
    async fn an_added_library_gets_the_pass_alone() {
        let rows = [("a", "fbx", None, "done", None)];
        let libs = [library("one", &rows).await, library("two", &rows).await];

        let run = run_libraries(&libs, Some("one"), Some(2), None, no_lock).await;

        assert_eq!(
            run,
            Run {
                requeued_in: 1,
                warning: None
            }
        );
        assert_eq!(queue(&libs[0]).await.0, strings(&["a"]));
        assert!(queue(&libs[1]).await.0.is_empty());
    }

    #[tokio::test]
    async fn publishing_a_current_version_runs_the_pass_for_every_library_still_below_it() {
        let rows = [("a", "fbx", None, "done", None)];
        let libs = [
            library("one", &rows).await,
            library("two", &rows).await,
            library("recorded", &rows).await,
        ];
        sqlx::query("INSERT INTO library_meta (key, value) VALUES ('model_preview_version', '2')")
            .execute(&libs[2].pool)
            .await
            .unwrap();
        let published = watch::Sender::new(None);
        let (runs_tx, mut runs) = tokio::sync::mpsc::unbounded_channel();
        let follow = {
            let (libs, published) = (&libs, &published);
            follow_engine(published.subscribe(), move || {
                let runs_tx = runs_tx.clone();
                async move {
                    let version = *published.borrow();
                    let run = run_libraries(libs, None, version, None, no_lock).await;
                    runs_tx.send(run).unwrap();
                }
            })
        };
        let check = async {
            // At startup, none yet
            assert_eq!(runs.recv().await.unwrap(), Run::default());
            published.send_replace(Some(1));
            assert!(runs.recv().await.unwrap().warning.is_some());
            for lib in &libs {
                assert!(queue(lib).await.0.is_empty());
            }

            // A restart onto a current engine
            published.send_replace(None);
            assert_eq!(runs.recv().await.unwrap(), Run::default());
            published.send_replace(Some(2));
            let run = runs.recv().await.unwrap();
            assert_eq!((run.requeued_in, run.warning), (2, None));
            assert_eq!(queue(&libs[0]).await.0, strings(&["a"]));
            assert_eq!(queue(&libs[1]).await.0, strings(&["a"]));
            assert!(queue(&libs[2]).await.0.is_empty());
        };
        tokio::select! {
            _ = follow => panic!("the publisher is still alive"),
            _ = check => {}
        }
    }

    #[test]
    fn the_checkout_worker_declares_the_current_version() {
        let worker = include_str!("../../../../packages/preview-engine/worker.py");
        let declared: Vec<&str> = worker
            .lines()
            .filter_map(|l| l.strip_prefix("PREVIEW_VERSION = "))
            .collect();
        assert_eq!(declared, [MODEL_PREVIEW_VERSION.to_string()]);
    }
}
