//! The import engine (plan pack-import, Runs), modeled on the autotag engine: it runs one import
//! run at a time from the durable queue in each library's `import_runs`.
//!
//! A run belongs to the process that queued it, which records itself as the run's owner. An
//! engine claims only its own queued runs, and those of a dead process on its own machine. While
//! a run is `running`, its task rewrites `heartbeat_at` every 30 s. At startup, a `running` run
//! whose owner is gone becomes `interrupted`: a dead process on this machine, or another
//! machine's run whose heartbeat stopped 10 minutes ago.
//!
//! Progress arrives as `import:progress` events and status changes as `import:updated`. Cancel
//! takes effect only before a build takes the library's lock (`build::RunControl`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Pool, Sqlite};
use tokio::sync::Notify;

use super::build::{self, BuildEnv, Hooks, Progress, RunControl, Stop};
use super::place::PlaceStep;
use super::records::{self, NewImport, Owner, QueuedPack, RunSummary, StepRecord};
use crate::db::{Database, LibraryHandle};

const HEARTBEAT: Duration = Duration::from_secs(30);
/// Another machine's run whose heartbeat is this old is gone.
const DEAD_AFTER: Duration = Duration::from_secs(10 * 60);
/// How long a build waits for another machine's advisory lock.
const LOCK_WAIT: Duration = Duration::from_secs(5 * 60);

/// One `import:progress` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProgress {
    pub run_id: String,
    pub library_id: String,
    pub pack_id: String,
    pub step: String,
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportEvent {
    /// `import:progress`
    Progress(ImportProgress),
    /// `import:updated`: a run's status changed.
    Updated(RunSummary),
    /// `library:updated`: a build changed the library.
    LibraryUpdated,
}

pub type EventSink = Arc<dyn Fn(ImportEvent) + Send + Sync>;

/// An Import pack request: a `.unitypackage` original, the pack's target folder relative to the
/// library root, and its publisher.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportRequest {
    pub library_id: String,
    pub original_path: String,
    pub target: String,
    pub publisher: String,
    /// The place steps, `flatten` alone when left out (the Unity preset's).
    #[serde(default)]
    pub place: Option<Vec<PlaceStep>>,
}

pub struct Engine {
    db: Arc<Database>,
    events: EventSink,
    wake: Notify,
    /// The run being executed, with its Cancel.
    active: Mutex<Option<(String, Arc<RunControl>)>>,
    owner: Owner,
    hooks: Hooks,
    lock_wait: Duration,
}

impl Engine {
    pub fn new(db: Arc<Database>, events: EventSink) -> Arc<Self> {
        Self::with_lock_wait(db, events, LOCK_WAIT)
    }

    fn with_lock_wait(db: Arc<Database>, events: EventSink, lock_wait: Duration) -> Arc<Self> {
        Arc::new(Engine {
            db,
            events,
            wake: Notify::new(),
            active: Mutex::new(None),
            owner: Owner::this_process(),
            hooks: Hooks::default(),
            lock_wait,
        })
    }

    #[cfg(test)]
    pub fn hooks(&self) -> &Hooks {
        &self.hooks
    }

    /// Marks the runs of gone owners `interrupted`, then runs queued runs one at a time, for good.
    pub async fn start(self: Arc<Self>) {
        for library in self.db.get_all_libraries().await {
            if let Err(e) = self.interrupt_dead_runs(&library).await {
                tracing::error!("Could not check the import runs of {}: {e}", library.id);
            }
        }
        loop {
            match self.next_run().await {
                Some((library, run)) => {
                    let (this, lib, run_id) = (self.clone(), library.clone(), run.id.clone());
                    // A panic ends the run, not the engine. A pack it left mid-swap keeps its
                    // journal, for recovery.
                    if let Err(e) = tokio::spawn(async move { this.execute(&lib, run).await }).await
                    {
                        tracing::error!("Import run {run_id} stopped: {e}");
                        self.set_active(None);
                        let _ = sqlx::query(
                            "UPDATE import_runs SET status = 'failed', finished_at = ?,
                             error = 'The run stopped unexpectedly' WHERE id = ?",
                        )
                        .bind(now_ms())
                        .bind(&run_id)
                        .execute(&library.pool)
                        .await;
                        self.updated(&library, &run_id).await;
                    }
                }
                // A run queued meanwhile has stored a permit, so this returns at once.
                None => self.wake.notified().await,
            }
        }
    }

    async fn interrupt_dead_runs(&self, library: &LibraryHandle) -> sqlx::Result<()> {
        #[allow(clippy::type_complexity)]
        let running: Vec<(String, Option<String>, i64, i64, Option<i64>)> = sqlx::query_as(
            "SELECT id, owner_machine, owner_pid, owner_started, heartbeat_at FROM import_runs
             WHERE status = 'running'",
        )
        .fetch_all(&library.pool)
        .await?;
        let now = now_ms();
        for (id, machine, pid, started, heartbeat) in running {
            let gone = if machine.is_some() && machine == self.owner.machine {
                !crate::watcher::process_alive(pid as u32, started as u64)
            } else {
                now - heartbeat.unwrap_or(0) >= DEAD_AFTER.as_millis() as i64
            };
            if !gone {
                continue;
            }
            sqlx::query(
                "UPDATE import_runs SET status = 'interrupted', finished_at = ?,
                 error = 'OpenDAM stopped during the run' WHERE id = ? AND status = 'running'",
            )
            .bind(now)
            .bind(&id)
            .execute(&library.pool)
            .await?;
            self.updated(library, &id).await;
        }
        Ok(())
    }

    /// The oldest queued run this engine may claim: its own, or a dead process's on this machine.
    async fn next_run(&self) -> Option<(LibraryHandle, Queued)> {
        for library in self.db.get_all_libraries().await {
            #[allow(clippy::type_complexity)]
            let queued: Vec<(String, String, String, Option<String>, i64, i64)> = match sqlx::query_as(
                "SELECT id, kind, packs, owner_machine, owner_pid, owner_started FROM import_runs
                 WHERE status = 'queued' ORDER BY created_at, rowid",
            )
            .fetch_all(&library.pool)
            .await
            {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::error!("Could not read the import queue of {}: {e}", library.id);
                    continue;
                }
            };
            for (id, kind, packs, machine, pid, started) in queued {
                let mine = machine == self.owner.machine
                    && pid as u32 == self.owner.pid
                    && started as u64 == self.owner.started;
                let orphan = machine.is_some()
                    && machine == self.owner.machine
                    && !crate::watcher::process_alive(pid as u32, started as u64);
                if mine || orphan {
                    let packs = serde_json::from_str(&packs).unwrap_or_default();
                    return Some((library, Queued { id, kind, packs }));
                }
            }
        }
        None
    }

    async fn execute(&self, library: &LibraryHandle, run: Queued) {
        let control = Arc::new(RunControl::default());
        self.set_active(Some((run.id.clone(), control.clone())));
        let claimed = sqlx::query(
            "UPDATE import_runs SET status = 'running', started_at = ?, heartbeat_at = ?,
             owner_machine = ?, owner_pid = ?, owner_started = ?
             WHERE id = ? AND status = 'queued'",
        )
        .bind(now_ms())
        .bind(now_ms())
        .bind(&self.owner.machine)
        .bind(self.owner.pid as i64)
        .bind(self.owner.started as i64)
        .bind(&run.id)
        .execute(&library.pool)
        .await;
        if !matches!(&claimed, Ok(done) if done.rows_affected() == 1) {
            // Cancelled while queued, or claimed by another engine.
            self.set_active(None);
            return;
        }
        self.updated(library, &run.id).await;

        let latest: Arc<Mutex<Option<Progress>>> = Default::default();
        let heartbeat = tokio::spawn(beat(library.pool.clone(), run.id.clone(), latest.clone()));
        let mut warnings = Vec::new();
        let mut outcome = Ok(());
        for pack in &run.packs {
            let progress: build::ProgressSink = Arc::new({
                let (events, latest) = (self.events.clone(), latest.clone());
                let (run_id, library_id, pack_id) =
                    (run.id.clone(), library.id.clone(), pack.pack_id.clone());
                move |p: Progress| {
                    *latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
                    events(ImportEvent::Progress(ImportProgress {
                        run_id: run_id.clone(),
                        library_id: library_id.clone(),
                        pack_id: pack_id.clone(),
                        step: p.step.to_string(),
                        done: p.done,
                        total: p.total,
                    }));
                }
            });
            let env = BuildEnv {
                db: &self.db,
                library,
                control: control.clone(),
                hooks: &self.hooks,
                progress,
                lock_wait: self.lock_wait,
            };
            outcome = match run.kind.as_str() {
                "import" => {
                    build::build_first(&env, &pack.pack_id, pack.recipe_rev, &mut warnings).await
                }
                other => Err(Stop::Failed(format!("{other} runs can't run yet"))),
            };
            if let Err(Stop::Failed(_)) = &outcome {
                let _ = sqlx::query(
                    "UPDATE import_packs SET status = 'failed'
                     WHERE id = ? AND current_build IS NULL",
                )
                .bind(&pack.pack_id)
                .execute(&library.pool)
                .await;
            }
            if outcome.is_err() {
                break;
            }
        }
        heartbeat.abort();

        let (status, error) = match &outcome {
            Ok(()) => ("done", None),
            Err(Stop::Cancelled) => ("cancelled", None),
            Err(Stop::Failed(message)) => ("failed", Some(message.clone())),
        };
        let phase = latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|p| p.step);
        let finished = sqlx::query(
            "UPDATE import_runs SET status = ?, error = ?, warnings = ?, phase = ?,
             finished_at = ? WHERE id = ?",
        )
        .bind(status)
        .bind(&error)
        .bind(json!(warnings).to_string())
        .bind(phase)
        .bind(now_ms())
        .bind(&run.id)
        .execute(&library.pool)
        .await;
        if let Err(e) = finished {
            tracing::error!("Could not record the end of import run {}: {e}", run.id);
        }
        if let Some(error) = &error {
            tracing::warn!("Import run {} failed: {error}", run.id);
        }
        self.set_active(None);
        if !matches!(outcome, Err(Stop::Cancelled)) {
            (self.events)(ImportEvent::LibraryUpdated);
        }
        self.updated(library, &run.id).await;
    }

    fn set_active(&self, active: Option<(String, Arc<RunControl>)>) {
        *self.active.lock().unwrap_or_else(|e| e.into_inner()) = active;
    }

    async fn updated(&self, library: &LibraryHandle, run_id: &str) {
        match records::run(&library.pool, &library.id, run_id).await {
            Ok(run) => (self.events)(ImportEvent::Updated(run)),
            Err(e) => tracing::error!("Could not read import run {run_id}: {e}"),
        }
    }

    /// Cancels a queued run, or the running one while its build hasn't taken the library's lock.
    /// After that it answers "Finishing, can't cancel now", and the build completes.
    pub async fn cancel_run(&self, library_id: &str, run_id: &str) -> Result<(), String> {
        let active = self
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some((id, control)) = active {
            if id == run_id {
                return control.cancel();
            }
        }
        let library = self.library(library_id).await?;
        let cancelled = sqlx::query(
            "UPDATE import_runs SET status = 'cancelled', finished_at = ?
             WHERE id = ? AND status = 'queued'",
        )
        .bind(now_ms())
        .bind(run_id)
        .execute(&library.pool)
        .await
        .map_err(|e| e.to_string())?;
        if cancelled.rows_affected() == 0 {
            return Err("The run is neither queued nor running".into());
        }
        self.updated(&library, run_id).await;
        Ok(())
    }

    pub async fn list_runs(&self, library_id: &str) -> Result<Vec<RunSummary>, String> {
        let library = self.library(library_id).await?;
        records::runs(&library.pool, &library.id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn library(&self, library_id: &str) -> Result<LibraryHandle, String> {
        self.db
            .get_library_handle(library_id)
            .await
            .ok_or_else(|| format!("Library {library_id} isn't open"))
    }

    /// Records a new import of `request.original_path` and queues its first build. Refuses a
    /// target that already exists in the library or nests with another pack's target.
    pub async fn start_import(&self, request: ImportRequest) -> Result<RunSummary, String> {
        let library = self.library(&request.library_id).await?;
        let root = library
            .root_path
            .canonicalize()
            .map_err(|e| format!("The library folder can't be read: {e}"))?;
        let target = clean_target(&request.target)?;
        let publisher = request.publisher.trim();
        if publisher.is_empty() {
            return Err("Name the pack's publisher".into());
        }
        let original = original(&request.original_path, &root)?;
        check_target(&library.pool, &root, &target).await?;

        let path = original.clone();
        let (size, mtime_ns, sha256) = tokio::task::spawn_blocking(move || {
            let meta = std::fs::metadata(&path)?;
            let mtime = meta
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let sha256 = crate::export::hashes::sha256_file(&path)?;
            Ok::<_, std::io::Error>((meta.len(), mtime.as_nanos() as i64, sha256))
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("The original can't be read: {e}"))?;

        let mut steps = vec![StepRecord {
            kind: "extract_unitypackage".into(),
            options: json!({}),
        }];
        let place = request.place.unwrap_or_else(|| vec![PlaceStep::Flatten]);
        steps.extend(place.iter().map(StepRecord::place));
        steps.push(StepRecord {
            kind: "metadata".into(),
            options: json!({ "preset": "unity", "publisher": publisher }),
        });
        let original_path = original.to_string_lossy();
        let import = NewImport {
            original_path: &original_path,
            original_size: size as i64,
            original_mtime_ns: mtime_ns,
            original_sha256: &sha256,
            kind: "unitypackage",
            target: &target,
            preset: "unity",
            steps: &steps,
        };
        let run_id = records::create_import(&library.pool, &import, &self.owner, now_ms())
            .await
            .map_err(|e| match e {
                sqlx::Error::Database(e) if e.is_unique_violation() => {
                    format!("Another import already builds {target}")
                }
                e => e.to_string(),
            })?;
        let run = records::run(&library.pool, &library.id, &run_id)
            .await
            .map_err(|e| e.to_string())?;
        (self.events)(ImportEvent::Updated(run.clone()));
        self.wake.notify_one();
        Ok(run)
    }
}

/// A queued run, as the engine claims it.
struct Queued {
    id: String,
    kind: String,
    packs: Vec<QueuedPack>,
}

/// Rewrites a running run's heartbeat, with its latest progress, every 30 s until aborted.
async fn beat(pool: Pool<Sqlite>, run_id: String, latest: Arc<Mutex<Option<Progress>>>) {
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    loop {
        ticks.tick().await;
        let progress = *latest.lock().unwrap_or_else(|e| e.into_inner());
        let _ = sqlx::query(
            "UPDATE import_runs SET heartbeat_at = ?, phase = COALESCE(?, phase),
             progress_done = ?, progress_total = ? WHERE id = ? AND status = 'running'",
        )
        .bind(now_ms())
        .bind(progress.map(|p| p.step))
        .bind(progress.map_or(0, |p| p.done as i64))
        .bind(progress.map_or(0, |p| p.total as i64))
        .bind(&run_id)
        .execute(&pool)
        .await;
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The target as a clean relative path: no `..`, no dot folder (which the index never walks), no
/// backslash.
fn clean_target(target: &str) -> Result<String, String> {
    let parts: Vec<&str> = target
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    let invalid = target.starts_with('/')
        || parts.is_empty()
        || parts.iter().any(|p| p.starts_with('.') || p.contains('\\'));
    if invalid {
        return Err(format!(
            "{target:?} isn't a folder in the library: give a relative path such as Publisher/Pack"
        ));
    }
    Ok(parts.join("/"))
}

/// The original as an absolute path: an existing `.unitypackage` outside the library, which
/// nothing ever writes.
fn original(path: &str, root: &Path) -> Result<PathBuf, String> {
    let original = Path::new(path)
        .canonicalize()
        .map_err(|e| format!("The original {path} can't be read: {e}"))?;
    if !original.is_file() {
        return Err(format!("The original {path} isn't a file"));
    }
    let unitypackage = original
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("unitypackage"));
    if !unitypackage {
        return Err(format!("{path} isn't a .unitypackage"));
    }
    if original.starts_with(root) {
        return Err(format!(
            "The original {path} is inside the library: an original stays outside it"
        ));
    }
    Ok(original)
}

/// Refuses a target that exists in the library, or that is another pack's target, or an
/// ancestor or descendant of one, on whole path components.
async fn check_target(pool: &Pool<Sqlite>, root: &Path, target: &str) -> Result<(), String> {
    if root.join(target).symlink_metadata().is_ok() {
        return Err(format!("{target} already exists in the library"));
    }
    let targets: Vec<String> = sqlx::query_scalar("SELECT target FROM import_packs")
        .fetch_all(pool)
        .await
        .map_err(|e| e.to_string())?;
    let nests = |a: &str, b: &str| a == b || b.starts_with(&format!("{a}/"));
    if let Some(other) = targets
        .iter()
        .find(|t| nests(t, target) || nests(target, t))
    {
        return Err(format!(
            "{target} would nest with the pack imported to {other}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{memory_database, memory_library_pool};
    use crate::fs_swap;
    use crate::import::build::{Point, FINISHING};
    use crate::import::unitypackage::tests::{fixture, golden, on_disk};

    const TARGET: &str = "Pub/Synthetic";

    struct Setup {
        dir: tempfile::TempDir,
        lib: LibraryHandle,
        engine: Arc<Engine>,
        events: Arc<Mutex<Vec<ImportEvent>>>,
    }

    impl Setup {
        fn live(&self) -> PathBuf {
            self.dir.path().join(TARGET)
        }

        fn request(&self, place: Vec<PlaceStep>) -> ImportRequest {
            ImportRequest {
                library_id: self.lib.id.clone(),
                original_path: fixture("synthetic.unitypackage")
                    .to_string_lossy()
                    .into_owned(),
                target: TARGET.into(),
                publisher: "Synthetic Studio".into(),
                place: Some(place),
            }
        }

        /// The import of `synthetic.unitypackage` whose output is the place golden.
        fn golden_request(&self) -> ImportRequest {
            self.request(vec![
                PlaceStep::Flatten,
                PlaceStep::Hoist {
                    folder: "Art".into(),
                },
            ])
        }

        async fn wait_for(&self, run_id: &str, status: &str) -> RunSummary {
            for _ in 0..1500 {
                let run = records::run(&self.lib.pool, &self.lib.id, run_id)
                    .await
                    .unwrap();
                if run.status == status {
                    return run;
                }
                assert!(
                    !["done", "failed", "cancelled"].contains(&run.status.as_str()),
                    "the run ended {} ({:?}), not {status}",
                    run.status,
                    run.error
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("the run never reached {status}");
        }

        async fn strings(&self, sql: &str) -> Vec<String> {
            sqlx::query_scalar(sql)
                .fetch_all(&self.lib.pool)
                .await
                .unwrap()
        }

        async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sql)
                .fetch_one(&self.lib.pool)
                .await
                .unwrap()
        }

        /// The pack's `(status, current_build, swap_state)`.
        async fn pack(&self) -> (String, Option<String>, Option<String>) {
            sqlx::query_as("SELECT status, current_build, swap_state FROM import_packs")
                .fetch_one(&self.lib.pool)
                .await
                .unwrap()
        }

        /// No staging, previous or failed folder is left beside the target.
        fn no_leftovers(&self) {
            for tag in ["import", "prev", "failed"] {
                let left = fs_swap::leftovers(&self.live(), tag).unwrap();
                assert!(left.is_empty(), "{left:?}");
            }
        }

        /// The tagged asset outside the pack is untouched.
        async fn other_asset_untouched(&self) {
            let tagged: Vec<(String, String)> = sqlx::query_as(
                "SELECT a.id, a.file_path FROM assets a JOIN asset_tags t ON t.asset_id = a.id",
            )
            .fetch_all(&self.lib.pool)
            .await
            .unwrap();
            assert_eq!(tagged, [("tree".to_string(), "Other/Tree.png".to_string())]);
        }
    }

    /// A library holding `Other/Tree.png`, indexed, with the row given the id `tree` and a tag,
    /// and an engine started over it.
    async fn setup() -> Setup {
        let dir = tempfile::Builder::new()
            .prefix("opendam-lib-")
            .tempdir()
            .unwrap();
        std::fs::create_dir_all(dir.path().join(".opendam")).unwrap();
        std::fs::create_dir_all(dir.path().join("Other")).unwrap();
        std::fs::write(dir.path().join("Other/Tree.png"), "png").unwrap();
        let lib = LibraryHandle {
            id: "lib".into(),
            name: "lib".into(),
            root_path: dir.path().to_path_buf(),
            pool: memory_library_pool().await,
        };
        let db = Arc::new(memory_database(vec![lib.clone()]).await);
        crate::watcher::index_paths(&db, "lib", &[dir.path().to_string_lossy().into_owned()])
            .await
            .unwrap();
        for sql in [
            "UPDATE assets SET id = 'tree' WHERE file_path = 'Other/Tree.png'",
            "INSERT INTO tags (id, name) VALUES ('t1', 'keep')",
            "INSERT INTO asset_tags (asset_id, tag_id) VALUES ('tree', 't1')",
        ] {
            sqlx::query(sql).execute(&lib.pool).await.unwrap();
        }
        let events: Arc<Mutex<Vec<ImportEvent>>> = Default::default();
        let sink = events.clone();
        let engine = Engine::with_lock_wait(
            db,
            Arc::new(move |event| sink.lock().unwrap().push(event)),
            Duration::from_secs(2),
        );
        tokio::spawn(engine.clone().start());
        Setup {
            dir,
            lib,
            engine,
            events,
        }
    }

    fn stamp(path: &Path) -> (String, std::time::SystemTime) {
        (
            crate::export::hashes::sha256_file(path).unwrap(),
            std::fs::metadata(path).unwrap().modified().unwrap(),
        )
    }

    #[tokio::test]
    async fn an_import_builds_the_golden_records_its_manifest_and_indexes_it() {
        let s = setup().await;
        let original = fixture("synthetic.unitypackage");
        let before = stamp(&original);

        let run = s.engine.start_import(s.golden_request()).await.unwrap();
        assert_eq!(run.status, "queued");
        s.wait_for(&run.id, "done").await;

        let expected = golden("synthetic.place.txt");
        assert_eq!(on_disk(&s.live()), expected);
        let in_pack: Vec<String> = expected.iter().map(|p| format!("{TARGET}/{p}")).collect();
        assert_eq!(
            s.strings(
                "SELECT o.path FROM import_outputs o JOIN import_packs p ON p.id = o.pack_id
                 AND o.build_id = p.current_build ORDER BY o.path"
            )
            .await,
            in_pack,
            "the manifest is the current build's rows"
        );
        assert_eq!(
            s.count("SELECT COUNT(*) FROM import_outputs").await,
            in_pack.len() as i64
        );
        let indexed: Vec<String> = in_pack
            .iter()
            .filter(|p| {
                let ext = p.rsplit_once('.').map_or("", |(_, e)| e);
                crate::watcher::is_supported(ext)
            })
            .cloned()
            .collect();
        assert!(indexed.iter().any(|p| p.ends_with("Models/SM_Log.fbx")));
        assert_eq!(
            s.strings(&format!(
                "SELECT file_path FROM assets WHERE file_path LIKE '{TARGET}/%' ORDER BY file_path"
            ))
            .await,
            indexed
        );
        assert_eq!(
            s.count(&format!(
                "SELECT COUNT(*) FROM folders WHERE path = '{TARGET}'"
            ))
            .await,
            1,
            "the settle ran the structure pass"
        );
        let (status, current_build, swap_state) = s.pack().await;
        assert_eq!((status.as_str(), swap_state), ("built", None));
        assert!(current_build.is_some());
        assert_eq!(s.count("SELECT COUNT(*) FROM import_moves").await, 0);
        let sha_matches = s
            .count(&format!(
                "SELECT COUNT(*) FROM import_outputs WHERE path = '{TARGET}/Readme.txt'
                 AND sha256 = '{}'",
                crate::export::hashes::sha256_file(&s.live().join("Readme.txt")).unwrap()
            ))
            .await;
        assert_eq!(sha_matches, 1);
        s.no_leftovers();
        s.other_asset_untouched().await;
        assert_eq!(stamp(&original), before, "the original is unchanged");
        let recorded = s.strings("SELECT original_path FROM imports").await;
        assert_eq!(
            recorded,
            [original.canonicalize().unwrap().to_string_lossy()]
        );

        let events = s.events.lock().unwrap().clone();
        let statuses: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ImportEvent::Updated(r) if r.id == run.id => Some(r.status.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(statuses, ["queued", "running", "done"]);
        let mut steps: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ImportEvent::Progress(p) => Some(p.step.as_str()),
                _ => None,
            })
            .collect();
        steps.dedup();
        assert_eq!(
            steps,
            [
                "extracting",
                "placing",
                "hashing",
                "waiting for the library",
                "swapping",
                "indexing",
                "settling"
            ]
        );
        assert!(events.contains(&ImportEvent::LibraryUpdated));
    }

    #[tokio::test]
    async fn cancel_before_the_lock_deletes_the_staging_folder_and_changes_nothing() {
        let s = setup().await;
        let assets = s.strings("SELECT id FROM assets ORDER BY id").await;
        let pause = s.engine.hooks().pause(Point::Staged);
        let run = s.engine.start_import(s.golden_request()).await.unwrap();
        pause.reached.notified().await;
        assert_eq!(fs_swap::leftovers(&s.live(), "import").unwrap().len(), 1);

        s.engine.cancel_run("lib", &run.id).await.unwrap();
        pause.release.notify_one();
        s.wait_for(&run.id, "cancelled").await;

        s.no_leftovers();
        assert!(
            !s.dir.path().join("Pub").exists(),
            "the folder made for the staging is gone"
        );
        assert_eq!(s.strings("SELECT id FROM assets ORDER BY id").await, assets);
        assert_eq!(s.count("SELECT COUNT(*) FROM import_outputs").await, 0);
        assert_eq!(s.pack().await, ("never_built".into(), None, None));
        s.other_asset_untouched().await;
        assert!(!s
            .events
            .lock()
            .unwrap()
            .contains(&ImportEvent::LibraryUpdated));
    }

    #[tokio::test]
    async fn cancel_after_the_lock_reads_finishing_and_the_build_completes() {
        let s = setup().await;
        let pause = s.engine.hooks().pause(Point::Indexed);
        let run = s.engine.start_import(s.golden_request()).await.unwrap();
        pause.reached.notified().await;

        assert_eq!(
            s.engine.cancel_run("lib", &run.id).await,
            Err(FINISHING.into())
        );
        pause.release.notify_one();
        s.wait_for(&run.id, "done").await;

        assert_eq!(on_disk(&s.live()), golden("synthetic.place.txt"));
        assert_eq!(s.pack().await.0, "built");
    }

    #[tokio::test]
    async fn a_failing_index_rolls_a_first_build_back_to_no_folder_and_no_rows() {
        let s = setup().await;
        let assets = s.strings("SELECT id FROM assets ORDER BY id").await;
        let pause = s.engine.hooks().pause(Point::Indexed);
        s.engine.hooks().fail(Point::Indexed);
        let run = s.engine.start_import(s.golden_request()).await.unwrap();
        pause.reached.notified().await;
        let inserted = s
            .count(&format!(
                "SELECT COUNT(*) FROM assets WHERE file_path LIKE '{TARGET}/%'"
            ))
            .await;
        assert!(inserted > 0, "the index inserted the pack's rows");
        assert_eq!(
            s.count("SELECT COUNT(*) FROM import_moves WHERE kind = 'inserted'")
                .await,
            inserted,
            "and journaled each"
        );
        assert_eq!(s.pack().await.2.as_deref(), Some("swapping"));
        pause.release.notify_one();

        let failed = s.wait_for(&run.id, "failed").await;
        assert!(
            failed
                .error
                .as_deref()
                .unwrap()
                .contains("injected failure"),
            "{failed:?}"
        );
        assert!(!s.live().exists() && !s.dir.path().join("Pub").exists());
        s.no_leftovers();
        assert_eq!(s.strings("SELECT id FROM assets ORDER BY id").await, assets);
        assert_eq!(s.count("SELECT COUNT(*) FROM import_moves").await, 0);
        assert_eq!(s.count("SELECT COUNT(*) FROM import_outputs").await, 0);
        assert_eq!(s.pack().await, ("failed".into(), None, None));
        assert_eq!(
            s.count("SELECT COUNT(*) FROM folders WHERE path LIKE 'Pub%'")
                .await,
            0
        );
        s.other_asset_untouched().await;
    }

    #[tokio::test]
    async fn a_staging_folder_changed_before_the_lock_fails_the_run_and_changes_nothing() {
        let s = setup().await;
        let assets = s.strings("SELECT id FROM assets ORDER BY id").await;
        let pause = s.engine.hooks().pause(Point::Staged);
        let run = s.engine.start_import(s.golden_request()).await.unwrap();
        pause.reached.notified().await;
        let staging = fs_swap::leftovers(&s.live(), "import").unwrap().remove(0);
        std::fs::remove_file(staging.join("Readme.txt")).unwrap();
        pause.release.notify_one();

        let failed = s.wait_for(&run.id, "failed").await;
        assert_eq!(
            failed.error.as_deref(),
            Some("The pack changed during the build; Rebuild again")
        );
        assert!(!s.live().exists());
        s.no_leftovers();
        assert_eq!(s.strings("SELECT id FROM assets ORDER BY id").await, assets);
        assert_eq!(s.count("SELECT COUNT(*) FROM import_outputs").await, 0);
        assert_eq!(s.pack().await, ("failed".into(), None, None));
    }

    #[tokio::test]
    async fn a_failed_move_in_clears_the_journal_and_changes_nothing() {
        let s = setup().await;
        s.engine.hooks().fail(Point::MoveIn);
        let run = s.engine.start_import(s.golden_request()).await.unwrap();

        s.wait_for(&run.id, "failed").await;
        assert!(!s.live().exists());
        s.no_leftovers();
        assert_eq!(s.count("SELECT COUNT(*) FROM import_outputs").await, 0);
        assert_eq!(s.pack().await, ("failed".into(), None, None));
        s.other_asset_untouched().await;
    }

    #[tokio::test]
    async fn a_target_that_exists_or_nests_or_isnt_a_folder_is_refused() {
        let s = setup().await;
        let mut request = s.golden_request();
        let refused = |target: &str| {
            let mut request = s.golden_request();
            request.target = target.into();
            s.engine.start_import(request)
        };

        let error = refused("Other").await.unwrap_err();
        assert_eq!(error, "Other already exists in the library");
        for target in ["", "../Escape", "/abs", "Pub/.hidden", "Pub\\Pack"] {
            assert!(refused(target).await.is_err(), "{target:?} was accepted");
        }
        request.original_path = s.dir.path().join("Other/Tree.png").to_string_lossy().into();
        assert!(s.engine.start_import(request).await.is_err());
        assert_eq!(s.count("SELECT COUNT(*) FROM imports").await, 0);

        // A pack recorded at Kits/Rock, with nothing on disk yet.
        for sql in [
            "INSERT INTO imports (id, original_path, original_size, original_mtime_ns,
             original_sha256, kind, created_at) VALUES ('i', '/o.unitypackage', 0, 0, '',
             'unitypackage', 0)",
            "INSERT INTO import_packs (id, import_id, target, preset)
             VALUES ('p', 'i', 'Kits/Rock', 'unity')",
        ] {
            sqlx::query(sql).execute(&s.lib.pool).await.unwrap();
        }
        for target in ["Kits", "Kits/Rock", "Kits/Rock/Sub"] {
            let error = refused(target).await.unwrap_err();
            assert_eq!(
                error,
                format!("{target} would nest with the pack imported to Kits/Rock")
            );
        }
        s.engine.hooks().pause(Point::Staged);
        refused("Kits/Rocks").await.unwrap();
    }

    #[tokio::test]
    async fn startup_interrupts_only_runs_whose_owner_is_gone() {
        let s = setup().await;
        let me = s.engine.owner.clone();
        let now = now_ms();
        let stale = now - DEAD_AFTER.as_millis() as i64 - 1;
        let runs: [(&str, Option<&str>, i64, i64, i64); 4] = [
            ("dead_here", me.machine.as_deref(), i32::MAX as i64, 1, now),
            (
                "mine",
                me.machine.as_deref(),
                me.pid as i64,
                me.started as i64,
                now,
            ),
            ("silent_there", Some("elsewhere"), 1, 1, stale),
            ("alive_there", Some("elsewhere"), 1, 1, now),
        ];
        for (id, machine, pid, started, heartbeat) in runs {
            sqlx::query(
                "INSERT INTO import_runs (id, kind, packs, status, created_at, owner_machine,
                 owner_pid, owner_started, heartbeat_at) VALUES (?, 'import', '[]', 'running',
                 0, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(machine)
            .bind(pid)
            .bind(started)
            .bind(heartbeat)
            .execute(&s.lib.pool)
            .await
            .unwrap();
        }

        s.engine.interrupt_dead_runs(&s.lib).await.unwrap();

        let interrupted = s
            .strings("SELECT id FROM import_runs WHERE status = 'interrupted' ORDER BY id")
            .await;
        if me.machine.is_some() {
            assert_eq!(interrupted, ["dead_here", "silent_there"]);
        } else {
            // Without a machine id, a run is judged by its heartbeat alone.
            assert_eq!(interrupted, ["silent_there"]);
        }
    }
}
