use anyhow::Result;
use chrono::Utc;
use futures_util::future::BoxFuture;
use sqlx::{Pool, Sqlite, SqliteConnection};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::db::{make_relative_path, Database, LibraryHandle};
use crate::models::AssetKind;
use crate::preview::is_native_image;

// ── Advisory lock guard ─────────────────────────────────────────────────────

/// A lock this old by its mtime is stale, so a holder that died without a heartbeat on another
/// machine stops blocking indexing.
const STALE_AFTER: Duration = Duration::from_secs(120);
/// How often a holder rewrites its timestamp, so a hold of any length never looks stale.
const HEARTBEAT: Duration = Duration::from_secs(30);
/// How long a new holder waits before it re-reads the file to see whether it won the race.
const SETTLE: Duration = Duration::from_millis(100);
/// How often a holder that waits for the lock (`acquire_within`) tries it again.
const RETRY: Duration = Duration::from_millis(250);

/// What the lock needs from the world: a clock, which processes on this machine are alive, and
/// who this process is. Tests inject their own.
#[derive(Clone)]
struct LockEnv {
    now: Arc<dyn Fn() -> SystemTime + Send + Sync>,
    /// Whether this machine runs a process with this pid and start time.
    alive: Arc<dyn Fn(u32, u64) -> bool + Send + Sync>,
    /// Where `alive` can answer: this machine, boot and pid namespace (`local_id`). `None` when
    /// unknown (or `/proc` can't be read), which turns the dead-process rule off.
    machine: Option<String>,
    pid: u32,
    started: u64,
    settle: Duration,
    heartbeat: Duration,
}

impl LockEnv {
    fn system() -> Self {
        static ME: OnceLock<(Option<String>, u64)> = OnceLock::new();
        let (machine, started) = ME
            .get_or_init(|| {
                let started = process_start_time(std::process::id());
                (started.and_then(|_| local_id()), started.unwrap_or(0))
            })
            .clone();
        LockEnv {
            now: Arc::new(SystemTime::now),
            alive: Arc::new(|pid, started| process_start_time(pid) == Some(started)),
            machine,
            pid: std::process::id(),
            started,
            settle: SETTLE,
            heartbeat: HEARTBEAT,
        }
    }

    /// A unique token naming this machine, process and process start time.
    fn new_token(&self) -> String {
        let machine = self.machine.as_deref().unwrap_or("-");
        format!("{machine} {} {} {}", self.pid, self.started, Uuid::new_v4())
    }

    /// Whether `token` names a process in this machine, boot and pid namespace that is gone.
    fn names_a_dead_process(&self, token: &str) -> bool {
        let mut parts = token.split(' ');
        let (Some(machine), Some(pid), Some(started)) = (parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        let (Ok(pid), Ok(started)) = (pid.parse::<u32>(), started.parse::<u64>()) else {
            return false;
        };
        self.machine.as_deref() == Some(machine) && !(self.alive)(pid, started)
    }
}

/// The space a pid and start time are comparable in, as the lock names it: this machine, this
/// boot and this pid namespace. The machine id alone isn't enough: VMs or containers cloned from
/// one image share it, and a sandboxed instance on the same host has its own pids. A start time is
/// counted from boot, so it compares only within one boot.
fn local_id() -> Option<String> {
    let machine = ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .map(|id| id.trim().to_string())
        .find(|id| !id.is_empty())?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let pid_ns = std::fs::read_link("/proc/self/ns/pid").ok()?;
    Some(local_id_of(
        &machine,
        boot.trim(),
        &pid_ns.to_string_lossy(),
    ))
}

/// Hashed with a key of the app's own, as machine-id(5) asks, since the lock file sits in a
/// shared library folder.
fn local_id_of(machine: &str, boot: &str, pid_ns: &str) -> String {
    let keyed = format!("com.opendam.desktop index.lock\n{machine}\n{boot}\n{pid_ns}");
    crate::export::hashes::sha256_bytes(keyed.as_bytes())[..32].to_string()
}

/// A process's start time in clock ticks since boot, field 22 of `/proc/<pid>/stat`, counted
/// after the last `)`, since `(comm)` may hold spaces or parentheses.
fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// The token on the lock file's first line.
fn read_token(path: &Path) -> std::io::Result<String> {
    let content = std::fs::read_to_string(path)?;
    Ok(content.lines().next().unwrap_or("").trim().to_string())
}

/// Writes the token and the clock's time, and sets the mtime to it too, since staleness is
/// judged by mtime.
fn write_lock(path: &Path, token: &str, env: &LockEnv) -> std::io::Result<()> {
    let now = (env.now)();
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let mut file = std::fs::File::create(path)?;
    file.write_all(format!("{token}\n{secs}\n").as_bytes())?;
    let _ = file.set_modified(now);
    Ok(())
}

/// A lock this process wrote and still means to hold.
struct HeldLock {
    path: PathBuf,
    token: String,
    lost: AtomicBool,
    /// Set by the release. Every read and write of the file goes under this mutex too: a heartbeat
    /// in flight never rewrites a released lock, and a re-read never sees a beat's rewrite half
    /// done (`File::create` empties the file before the token is written back).
    released: Mutex<bool>,
}

/// What re-reading the lock file found.
#[derive(Debug, PartialEq)]
enum Reread {
    Held,
    /// Another token, or no file: marked lost.
    Lost,
    /// A read error other than a missing file, maybe a share's hiccup.
    Unreadable,
}

impl HeldLock {
    /// Re-reads the file, and marks the lock lost once it holds another token or none.
    fn reread(&self) -> Reread {
        if self.lost.load(Ordering::SeqCst) {
            return Reread::Lost;
        }
        let found = match read_token(&self.path) {
            Ok(found) if found == self.token => return Reread::Held,
            Ok(found) => found,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "no lock".into(),
            Err(e) => {
                tracing::warn!("Could not read index.lock at {:?}: {e}", self.path);
                return Reread::Unreadable;
            }
        };
        self.lost.store(true, Ordering::SeqCst);
        tracing::warn!(
            "index.lock at {:?} holds {found:?}, not this run's token: the lock is lost",
            self.path
        );
        Reread::Lost
    }

    /// One heartbeat: rewrites the timestamp while the file still holds this token. An unreadable
    /// file is read again at the next beat.
    fn beat(&self, env: &LockEnv) {
        let released = self.released.lock().unwrap_or_else(|e| e.into_inner());
        if *released || self.reread() != Reread::Held {
            return;
        }
        if let Err(e) = write_lock(&self.path, &self.token, env) {
            tracing::warn!("Could not refresh index.lock at {:?}: {e}", self.path);
        }
    }
}

/// RAII guard over `.opendam/index.lock`, which keeps two machines (or two processes) from
/// indexing the same library at once, since interleaved batch writes would corrupt the catalogue.
///
/// The file holds a token naming its holder's machine (with its boot and pid namespace), process
/// and process start time, and a timestamp that a heartbeat rewrites every 30 s, so a hold of any
/// length stays fresh. A lock is stale once its mtime is 120 s old, or at once when its token
/// names a process in this machine, boot and pid namespace that is gone. Dropping the guard stops
/// the heartbeat and deletes the file only while it still holds the guard's token.
///
/// Holding one, with the library's `index_lock`, is the proof the `_locked` variants
/// (`index_paths_locked`, `structure::rebuild_library_locked`) take instead of locking. Every
/// holder lets go through `release`, which runs a structure pass left pending meanwhile.
pub struct IndexLock {
    path: PathBuf,
    /// `None` when the file couldn't be written: the run goes on unlocked, as it always has.
    held: Option<Arc<HeldLock>>,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}

impl IndexLock {
    /// The lock file of the library rooted at `root`.
    fn path(root: &Path) -> PathBuf {
        root.join(".opendam").join("index.lock")
    }

    /// Try to acquire the advisory lock of the library rooted at `root`. Returns `None` while
    /// another holder has it.
    pub async fn try_acquire(root: &Path) -> Option<Self> {
        Self::try_acquire_with(&Self::path(root), STALE_AFTER, &LockEnv::system()).await
    }

    /// `try_acquire`, trying again until `wait` has passed while another holder has the lock.
    pub async fn acquire_within(root: &Path, wait: Duration) -> Option<Self> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(lock) = Self::try_acquire(root).await {
                return Some(lock);
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            tokio::time::sleep(RETRY.min(left)).await;
        }
    }

    /// Lets go of the lock, first running the structure pass on the library DB at `pool`, whose
    /// files lie below `root`, when one is pending (`structure::mark_pending`) and the file still
    /// holds this guard's token. A pass pending only because this holder's own pass just failed is
    /// left to the next entry point (`structure::owed_to_release`). The caller still holds the
    /// library's `index_lock`.
    pub async fn release(self, pool: &Pool<Sqlite>, root: &Path) {
        debug_assert!(self.path == Self::path(root));
        if self.still_held() && crate::structure::owed_to_release(pool).await {
            if let Err(e) = crate::structure::rebuild(pool, root).await {
                tracing::error!(target: "structure", "Pending structure pass failed for library at {root:?}: {e}");
            }
        }
    }

    async fn try_acquire_with(
        lock_path: &Path,
        stale_after: Duration,
        env: &LockEnv,
    ) -> Option<Self> {
        let lock_path = lock_path.to_path_buf();

        if let Ok(meta) = std::fs::metadata(&lock_path) {
            let token = read_token(&lock_path).unwrap_or_default();
            if env.names_a_dead_process(&token) {
                tracing::info!("index.lock names a process that is gone ({token}), taking it.");
            } else if let Ok(modified) = meta.modified() {
                // An mtime ahead of this clock counts as fresh.
                let age = (env.now)().duration_since(modified).unwrap_or_default();
                if age < stale_after {
                    tracing::info!(
                        "index.lock is {}s old - another instance is indexing, skipping.",
                        age.as_secs()
                    );
                    return None;
                }
            }
        }

        let token = env.new_token();
        if let Err(e) = write_lock(&lock_path, &token, env) {
            // Couldn't write - the share may be read-only. Log and proceed anyway (best-effort:
            // don't block indexing just because we can't lock).
            tracing::warn!(
                "Could not write index.lock at {:?}: {} - proceeding without lock",
                lock_path,
                e
            );
            return Some(IndexLock {
                path: lock_path,
                held: None,
                heartbeat: None,
            });
        }

        // Another instance may have checked and written at the same moment: the last write wins.
        tokio::time::sleep(env.settle).await;
        match read_token(&lock_path) {
            Ok(found) if found == token => {}
            found => {
                tracing::info!("index.lock was taken by another instance ({found:?}), skipping.");
                return None;
            }
        }

        let held = Arc::new(HeldLock {
            path: lock_path.clone(),
            token,
            lost: AtomicBool::new(false),
            released: Mutex::new(false),
        });
        let heartbeat = tokio::spawn({
            let (held, env) = (held.clone(), env.clone());
            async move {
                let start = tokio::time::Instant::now() + env.heartbeat;
                let mut ticks = tokio::time::interval_at(start, env.heartbeat);
                ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticks.tick().await;
                    let (held, env) = (held.clone(), env.clone());
                    let _ = tokio::task::spawn_blocking(move || held.beat(&env)).await;
                }
            }
        });
        Some(IndexLock {
            path: lock_path,
            held: Some(held),
            heartbeat: Some(heartbeat),
        })
    }

    /// Whether a heartbeat found another token in the file. A lost holder changes nothing more.
    #[cfg(test)]
    fn is_lost(&self) -> bool {
        self.held
            .as_ref()
            .is_some_and(|h| h.lost.load(Ordering::SeqCst))
    }

    /// Re-reads the file right before a change the lock guards, rather than trusting the last
    /// heartbeat, which may be 30 s old or, after a suspend, older than the lock's staleness. True
    /// while the file still holds this guard's token, or when the file couldn't be written at all.
    fn still_held(&self) -> bool {
        self.held.as_ref().is_none_or(|h| {
            let _beat = h.released.lock().unwrap_or_else(|e| e.into_inner());
            h.reread() == Reread::Held
        })
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
        let Some(held) = &self.held else { return };
        let mut released = held.released.lock().unwrap_or_else(|e| e.into_inner());
        *released = true;
        if read_token(&held.path).is_ok_and(|found| found == held.token) {
            let _ = std::fs::remove_file(&held.path);
        }
    }
}

// Supported extensions, grouped by category. rustfmt would pack the rows and
// leave each category comment trailing the row above it.
#[rustfmt::skip]
const SUPPORTED_EXTENSIONS: &[&str] = &[
    // 3D & CAD
    "fbx", "obj", "3ds", "dae", "gltf", "glb", "skp", "blend",
    "c4d", "3dm", "rfa", "rvt", "dwg", "dxf", "stl", "usd", "usdz", "max",
    "abc", "ply", "x3d", "wrl",
    // Materials
    "mat", "vrmat", "sbsar", "sbs", "mdl",
    // Textures
    "jpg", "jpeg", "png", "tga", "tif", "tiff", "bmp", "dds",
    "ktx", "ktx2", "webp", "avif",
    // HDRI/HDR
    "hdr", "exr", "hdri",
    // IES
    "ies", "ldt",
    // Video
    "mp4", "mov", "avi", "mkv", "webm", "m4v",
    // Image layered
    "psd", "psb", "ai", "svg", "gif",
    // Docs
    "pdf", "txt", "md",
];

pub fn is_supported(ext: &str) -> bool {
    SUPPORTED_EXTENSIONS.contains(&ext.to_lowercase().as_str())
}

/// Check whether a file is an Autodesk Revit backup file (e.g. `filename.0001.rfa` or `filename.0002.rvt`).
pub fn is_revit_backup(file_name: &str) -> bool {
    let lower = file_name.to_lowercase();
    if lower.ends_with(".rfa") || lower.ends_with(".rvt") {
        if let Some((stem, _ext)) = lower.rsplit_once('.') {
            if let Some((_base, suffix)) = stem.rsplit_once('.') {
                return (3..=5).contains(&suffix.len())
                    && suffix.chars().all(|c| c.is_ascii_digit());
            }
        }
    }
    false
}

struct PendingAsset {
    id: String,
    file_path: String,
    file_name: String,
    extension: String,
    kind_str: &'static str,
    size_bytes: i64,
    modified_at: i64,
    now: i64,
    preview_status: &'static str,
    thumbnail_path: Option<String>,
    is_update: bool,
}

/// How an index run treats the rows in its scope whose files it didn't find (`index_pool`).
#[derive(Clone, Copy)]
pub enum Prune<'a> {
    /// Delete them, then run the structure pass. Every run but an import build's.
    Now,
    /// Delete no row and run no structure pass, and report each row the run inserts to the
    /// journal, inside the transaction that inserts it. An import build runs this between its
    /// swap and its commit (plan pack-import, "Pruning waits for the commit").
    Defer(&'a dyn InsertJournal),
}

/// Records the rows a `Prune::Defer` run inserts.
pub trait InsertJournal: Sync {
    /// Called once per write batch that inserts rows, inside that batch's transaction, with each
    /// inserted row's `(id, file_path)`. An error rolls the batch back and fails the run.
    fn record<'a>(
        &'a self,
        tx: &'a mut SqliteConnection,
        inserted: &'a [(&'a str, &'a str)],
    ) -> BoxFuture<'a, sqlx::Result<()>>;
}

/// Library `library_id`'s handle while it is open and its root is mounted. An offline library is
/// skipped: walking a missing root finds nothing, and the prune would then remove every row.
async fn mounted(db: &Database, library_id: &str) -> Option<LibraryHandle> {
    let Some(library) = db.get_library_handle(library_id).await else {
        tracing::info!("Library {} is not open, skipping indexing", library_id);
        return None;
    };
    if !library.root_path.exists() {
        tracing::info!(
            "Library {} root {:?} is missing, skipping indexing",
            library_id,
            library.root_path
        );
        return None;
    }
    Some(library)
}

/// Takes library `library_id`'s `index_lock`, then the advisory lock, runs swap recovery and
/// `index_paths_locked` with `Prune::Now`, then releases. Yields when another machine holds the
/// advisory lock.
pub async fn index_paths(db: &Database, library_id: &str, paths: &[String]) -> Result<()> {
    let Some(library) = mounted(db, library_id).await else {
        return Ok(());
    };

    // ── This app's lock: a command's structure pass waits for the run's own ──
    let index_lock = db.index_lock(library_id);
    let index_guard = index_lock.lock().await;

    // ── Advisory lock: skip if another machine is already indexing ────────
    let Some(lock) = IndexLock::try_acquire(&library.root_path).await else {
        return Ok(()); // another instance is indexing — yield
    };

    let (pool, root) = (&library.pool, library.root_path.as_path());
    crate::import::recover_swaps(pool, root, crate::import::Recovery::Entry).await;
    let run = index_paths_locked(db, library_id, paths, Prune::Now, &index_guard, &lock).await;
    lock.release(pool, root).await;
    run
}

/// `index_paths` for a caller that holds library `library_id`'s `index_lock` and advisory lock,
/// passed as proof: it never locks either again. Runs `index_pool` on the library, then stores
/// its asset count.
pub async fn index_paths_locked(
    db: &Database,
    library_id: &str,
    paths: &[String],
    prune: Prune<'_>,
    index_guard: &tokio::sync::MutexGuard<'_, ()>,
    lock: &IndexLock,
) -> Result<()> {
    debug_assert!(std::ptr::eq(
        tokio::sync::MutexGuard::mutex(index_guard),
        &*db.index_lock(library_id)
    ));
    let Some(LibraryHandle {
        pool, root_path, ..
    }) = mounted(db, library_id).await
    else {
        return Ok(());
    };
    debug_assert!(lock.path == IndexLock::path(&root_path));

    index_pool(&pool, &root_path, paths, prune, Some(lock)).await?;

    // ── Update asset count ──────────────────────────────────────────────
    let count_row: Option<(i64,)> = sqlx::query_as("SELECT COUNT(*) FROM assets")
        .fetch_optional(&pool)
        .await?;
    let total_count = count_row.map(|(c,)| c).unwrap_or(0);

    let _ = sqlx::query("UPDATE libraries SET asset_count = ? WHERE id = ?")
        .bind(total_count)
        .bind(library_id)
        .execute(&pool)
        .await;

    let _ = sqlx::query("UPDATE libraries SET asset_count = ? WHERE id = ?")
        .bind(total_count)
        .bind(library_id)
        .execute(&db.master_pool)
        .await;

    tracing::info!(
        "NAS-efficient indexing complete for library {}: {} total assets",
        library_id,
        total_count
    );
    Ok(())
}

/// The library-relative paths whose rows a run over `paths` reads and may prune, or `None` when
/// one of them is the library root itself, which covers every row.
fn scopes(root: &Path, paths: &[String]) -> Option<Vec<String>> {
    paths
        .iter()
        .map(|p| make_relative_path(root, Path::new(p)))
        .map(|rel| (!rel.is_empty()).then_some(rel))
        .collect()
}

/// The rows of the library DB at `pool` that a run over `paths` reads: every row for a run over
/// the root, else those at or under a walked path, on whole path components (`Pub/Pack` holds
/// `Pub/Pack/Rock.fbx` but not `Pub/Package/Rock.fbx`).
async fn rows_in_scope(
    pool: &Pool<Sqlite>,
    root: &Path,
    paths: &[String],
) -> sqlx::Result<Vec<(String, String, i64, i64, Option<String>)>> {
    const COLUMNS: &str =
        "SELECT id, file_path, modified_at, size_bytes, thumbnail_path FROM assets";
    let Some(scopes) = scopes(root, paths) else {
        return sqlx::query_as(COLUMNS).fetch_all(pool).await;
    };
    let mut rows = Vec::new();
    for scope in scopes {
        // Every path below `scope` sorts between `scope/` and `scope0`, '0' being '/' + 1, and
        // `file_path` compares bytewise (BINARY), so this is an index range, not a scan.
        rows.extend(
            sqlx::query_as(&format!(
                "{COLUMNS} WHERE file_path = ? OR (file_path >= ? AND file_path < ?)"
            ))
            .bind(&scope)
            .bind(format!("{scope}/"))
            .bind(format!("{scope}0"))
            .fetch_all(pool)
            .await?,
        );
    }
    Ok(rows)
}

/// Walk `paths` and index all found files as Assets into the library DB at `pool`, whose files
/// lie below `root`. The lock-free core of `index_paths`: the caller holds the library's locks,
/// passing the advisory one as `advisory`, or owns a pool no one else can reach yet.
/// Highly optimized for NAS:
/// 1. Single initial query loads the existing assets under `paths` into RAM (0 queries during
///    walk). Rows elsewhere in the library are never read, so never pruned.
/// 2. Skips `.opendam` directory.
/// 3. Reuses each asset's existing thumbnail from the DB to never re-render previews across network.
/// 4. Groups writes into chunks of 200 inside single transactions (cutting NAS I/O by 99%).
/// 5. With `Prune::Now`, deletes the rows of files it didn't find, unless `advisory` no longer
///    holds its token, then ends with the structure pass (`structure::rebuild`).
pub async fn index_pool(
    pool: &Pool<Sqlite>,
    root_path: &Path,
    paths: &[String],
    prune: Prune<'_>,
    advisory: Option<&IndexLock>,
) -> Result<()> {
    if !root_path.exists() {
        tracing::info!("Library root {:?} is missing, skipping indexing", root_path);
        return Ok(());
    }
    let journal = match prune {
        Prune::Now => None,
        Prune::Defer(journal) => Some(journal),
    };
    let now = Utc::now().timestamp_millis();

    // ── 1. Load existing assets in scope in ONE query per walked path ────
    let existing_rows = rows_in_scope(pool, root_path, paths)
        .await
        .unwrap_or_default();

    let mut existing_map: HashMap<String, (String, i64, i64, Option<String>)> =
        HashMap::with_capacity(existing_rows.len());
    for (id, file_path, modified_at, size_bytes, thumb) in existing_rows {
        existing_map.insert(file_path, (id, modified_at, size_bytes, thumb));
    }

    let mut seen_paths: HashSet<String> = HashSet::with_capacity(existing_map.len() + 100);
    let mut pending_batch: Vec<PendingAsset> = Vec::with_capacity(200);

    // ── 2. Walk directory tree ──────────────────────────────────────────
    for root in paths {
        let root_dir = Path::new(root);
        let walker = WalkDir::new(root_dir)
            .follow_links(true)
            .into_iter()
            .filter_entry(|e| {
                // Skip hidden folders and .opendam internal folder
                let file_name = e.file_name().to_string_lossy();
                if e.file_type().is_dir() && file_name.starts_with('.') {
                    return false;
                }
                true
            });

        for entry in walker
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            // A pack's `datapackage.json` is the only JSON file indexed.
            if !is_supported(&ext) && !crate::structure::is_package_file(&file_name) {
                continue;
            }

            // Skip Revit backup files (e.g. name.0001.rfa, name.0002.rvt)
            if is_revit_backup(&file_name) {
                continue;
            }

            let rel_path = make_relative_path(root_path, path);
            seen_paths.insert(rel_path.clone());

            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let size_bytes = meta.len() as i64;
            let modified_at = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(now);

            // In-memory cache check: if unchanged, SKIP completely!
            if let Some((_, cached_mod, cached_size, _)) = existing_map.get(&rel_path) {
                if *cached_mod == modified_at && *cached_size == size_bytes {
                    continue; // 0 disk writes, 0 SQL queries!
                }
            }

            let kind = AssetKind::from_extension(&ext);
            let kind_str = kind.as_str();

            let mut preview_status = "none";
            let mut thumbnail_path = None;

            if crate::preview::needs_preview(&ext) {
                preview_status = "pending";
            }

            let (id, is_update) =
                if let Some((existing_id, _, _, existing_thumb)) = existing_map.get(&rel_path) {
                    // A changed raster is re-previewed (see `commit_asset_batch`): it may have
                    // crossed THUMB_SIZE either way. Other kinds keep their preview.
                    if !is_native_image(&ext) && existing_thumb.is_some() {
                        thumbnail_path = existing_thumb.clone();
                        preview_status = "done";
                    }
                    (existing_id.clone(), true)
                } else {
                    // A new file gets no job here: the structure pass this run ends with queues
                    // it once it knows the file's format group, so a member that isn't its
                    // group's main item is never queued, not even while this run goes on (plan
                    // C15, `structure::apply_previews`).
                    preview_status = "none";
                    (Uuid::new_v4().to_string(), false)
                };

            pending_batch.push(PendingAsset {
                id,
                file_path: rel_path,
                file_name,
                extension: ext,
                kind_str,
                size_bytes,
                modified_at,
                now,
                preview_status,
                thumbnail_path,
                is_update,
            });

            // Flush chunk if batch reaches 200 items
            if pending_batch.len() >= 200 {
                commit_asset_batch(pool, &pending_batch, journal).await?;
                pending_batch.clear();
            }
        }
    }

    // Flush any remaining assets
    if !pending_batch.is_empty() {
        commit_asset_batch(pool, &pending_batch, journal).await?;
        pending_batch.clear();
    }

    if journal.is_some() {
        return Ok(());
    }

    // ── 3. Detect and remove deleted files in batch ───────────────────────
    let to_delete: Vec<String> = existing_map
        .into_iter()
        .filter(|(rel, _)| !seen_paths.contains(rel))
        .map(|(_, (id, _, _, _))| id)
        .collect();
    prune_missing(pool, &to_delete, advisory).await;

    // ── 4. Derive the folder tree, still under the caller's locks ─────────
    // A failed pass is logged, not returned: the run's writes went through, so the caller still
    // emits `library:updated`, and the next pass derives the folders again.
    if let Err(e) = crate::structure::rebuild(pool, root_path).await {
        tracing::error!(target: "structure", "Structure pass failed for library at {root_path:?}: {e}");
    }
    Ok(())
}

/// Deletes the rows of files the run didn't find, unless the run lost its lock (or can't read it
/// now): another holder may be mid-change, so a file this run missed may only be moving.
async fn prune_missing(pool: &sqlx::Pool<sqlx::Sqlite>, ids: &[String], lock: Option<&IndexLock>) {
    if lock.is_some_and(|lock| !lock.still_held()) {
        tracing::warn!(
            "This index run doesn't hold its index.lock, so it prunes none of {} missing files",
            ids.len()
        );
        return;
    }
    for chunk in ids.chunks(200) {
        let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM assets WHERE id IN ({})", placeholders);
        let mut query = sqlx::query(&sql);
        for id in chunk {
            query = query.bind(id);
        }
        let _ = query.execute(pool).await;
    }
}

/// Helper to execute a batch of assets inside a single database transaction, reporting the rows
/// it inserts to `journal` inside it.
async fn commit_asset_batch(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    batch: &[PendingAsset],
    journal: Option<&dyn InsertJournal>,
) -> Result<()> {
    let mut tx = pool.begin().await?;

    for item in batch {
        if item.is_update && is_native_image(&item.extension) {
            // A changed raster gets a fresh preview, and keeps its old thumbnail until then. A job
            // that hasn't started reads the new file, so it is reused. A 'processing' one may be
            // decoding the old pixels, so the change gets a job of its own.
            sqlx::query(
                "UPDATE assets SET size_bytes = ?, modified_at = ?, preview_status = 'pending' WHERE id = ?",
            )
            .bind(item.size_bytes)
            .bind(item.modified_at)
            .bind(&item.id)
            .execute(&mut *tx)
            .await?;

            sqlx::query(&format!(
                "INSERT INTO preview_jobs (id, asset_id, status, created_at)
                 SELECT ?, ?, 'pending', ? WHERE NOT EXISTS (
                     SELECT 1 FROM preview_jobs WHERE asset_id = ?
                     AND status IN {})",
                crate::preview::WAITING_JOBS
            ))
            .bind(Uuid::new_v4().to_string())
            .bind(&item.id)
            .bind(item.now)
            .bind(&item.id)
            .execute(&mut *tx)
            .await?;
        } else if item.is_update {
            sqlx::query(
                r#"UPDATE assets SET
                    size_bytes = ?,
                    modified_at = ?,
                    preview_status = CASE WHEN preview_status = 'done' THEN 'done' ELSE ? END,
                    thumbnail_path = COALESCE(?, thumbnail_path)
                   WHERE id = ?"#,
            )
            .bind(item.size_bytes)
            .bind(item.modified_at)
            .bind(item.preview_status)
            .bind(&item.thumbnail_path)
            .bind(&item.id)
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query(
                r#"INSERT INTO assets
                   (id, file_path, file_name, extension, kind, size_bytes,
                    modified_at, indexed_at, rating, color_label, is_favorite, description,
                    thumbnail_path, preview_status, metadata)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, 'none', 0, '', ?, ?, '{}')"#,
            )
            .bind(&item.id)
            .bind(&item.file_path)
            .bind(&item.file_name)
            .bind(&item.extension)
            .bind(item.kind_str)
            .bind(item.size_bytes)
            .bind(item.modified_at)
            .bind(item.now)
            .bind(&item.thumbnail_path)
            .bind(item.preview_status)
            .execute(&mut *tx)
            .await?;
        }
    }

    if let Some(journal) = journal {
        let inserted: Vec<(&str, &str)> = batch
            .iter()
            .filter(|item| !item.is_update)
            .map(|item| (item.id.as_str(), item.file_path.as_str()))
            .collect();
        if !inserted.is_empty() {
            journal.record(&mut tx, &inserted).await?;
        }
    }

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{memory_database, memory_library_pool};
    use sqlx::{Pool, Sqlite};

    async fn library_with_one_asset(id: &str, root_path: PathBuf) -> LibraryHandle {
        let pool = memory_library_pool().await;
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
             VALUES ('a1', 'models/tree.glb', 'tree.glb', 'glb', 'model', 1024, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        LibraryHandle {
            id: id.to_string(),
            name: id.to_string(),
            root_path,
            pool,
        }
    }

    async fn asset_count(pool: &Pool<Sqlite>) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM assets")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn missing_root() -> PathBuf {
        std::env::temp_dir().join(format!("opendam-missing-{}", Uuid::new_v4()))
    }

    #[tokio::test]
    async fn indexing_an_offline_library_leaves_other_libraries_intact() {
        let online_root = std::env::temp_dir().join(format!("opendam-online-{}", Uuid::new_v4()));
        std::fs::create_dir_all(online_root.join(".opendam")).unwrap();
        let online = library_with_one_asset("online", online_root.clone()).await;
        let online_pool = online.pool.clone();
        // The offline library's root is missing, so Database::new never opened it.
        let db = memory_database(vec![online]).await;

        let offline_paths = [missing_root().to_string_lossy().to_string()];
        index_paths(&db, "offline", &offline_paths).await.unwrap();

        let remaining = asset_count(&online_pool).await;
        std::fs::remove_dir_all(&online_root).unwrap();
        assert_eq!(remaining, 1);
    }

    #[tokio::test]
    async fn indexing_an_open_library_whose_root_went_missing_leaves_its_rows_intact() {
        let root = missing_root();
        let unmounted = library_with_one_asset("unmounted", root.clone()).await;
        let pool = unmounted.pool.clone();
        let db = memory_database(vec![unmounted]).await;

        index_paths(&db, "unmounted", &[root.to_string_lossy().to_string()])
            .await
            .unwrap();

        assert_eq!(asset_count(&pool).await, 1);
    }

    #[tokio::test]
    async fn indexing_an_open_library_whose_root_exists_still_reindexes_it() {
        let root = std::env::temp_dir().join(format!("opendam-mounted-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".opendam")).unwrap();
        std::fs::write(root.join("notes.txt"), "hello").unwrap();
        // The seeded models/tree.glb row has no file on disk, so a re-index removes it.
        let mounted = library_with_one_asset("mounted", root.clone()).await;
        let pool = mounted.pool.clone();
        let db = memory_database(vec![mounted]).await;

        let result = index_paths(&db, "mounted", &[root.to_string_lossy().to_string()]).await;
        let file_paths: Vec<String> = sqlx::query_scalar("SELECT file_path FROM assets")
            .fetch_all(&pool)
            .await
            .unwrap();
        std::fs::remove_dir_all(&root).unwrap();

        result.unwrap();
        assert_eq!(file_paths, ["notes.txt"]);
    }

    #[tokio::test]
    async fn an_index_run_ends_with_the_folder_tree_of_what_it_found() {
        let (dir, db, lib) = temp_library().await;
        std::fs::create_dir_all(dir.path().join("kits/rocks")).unwrap();
        std::fs::write(dir.path().join("kits/rocks/rock.glb"), "glTF").unwrap();
        std::fs::write(dir.path().join("kits/readme.txt"), "hi").unwrap();
        reindex(&db, &lib).await;
        let first = crate::structure::tests::derived_rows(&lib.pool).await;

        std::fs::remove_dir_all(dir.path().join("kits/rocks")).unwrap();
        reindex(&db, &lib).await;

        assert_eq!(
            first,
            crate::structure::tests::fresh_pass(&["kits/rocks/rock.glb", "kits/readme.txt"]).await
        );
        assert_eq!(
            crate::structure::tests::derived_rows(&lib.pool).await,
            crate::structure::tests::fresh_pass(&["kits/readme.txt"]).await
        );
    }

    #[tokio::test]
    async fn an_index_run_whose_structure_pass_fails_still_succeeds() {
        let (dir, db, lib) = temp_library().await;
        std::fs::write(dir.path().join("notes.txt"), "hi").unwrap();
        // The pass reads `folders` first, so without it the pass fails after the run's writes.
        sqlx::query("DROP TABLE folders")
            .execute(&lib.pool)
            .await
            .unwrap();

        let result =
            index_paths(&db, &lib.id, &[lib.root_path.to_string_lossy().to_string()]).await;

        let file_paths: Vec<String> = sqlx::query_scalar("SELECT file_path FROM assets")
            .fetch_all(&lib.pool)
            .await
            .unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(file_paths, ["notes.txt"]);
    }

    /// A kit's model in three format folders, and a model on its own.
    fn write_grouped_kit(root: &Path) -> [&'static str; 4] {
        let paths = [
            "kit/Models/GLB format/rock.glb",
            "kit/Models/FBX format/rock.fbx",
            "kit/Models/OBJ format/rock.obj",
            "kit/tree.glb",
        ];
        for path in paths {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "model").unwrap();
        }
        paths
    }

    /// Every asset's (path, preview status), and every preview job's (asset path, status) of any
    /// status, by path.
    async fn queued(pool: &Pool<Sqlite>) -> (Vec<(String, String)>, Vec<(String, String)>) {
        let assets =
            sqlx::query_as("SELECT file_path, preview_status FROM assets ORDER BY file_path")
                .fetch_all(pool)
                .await
                .unwrap();
        let jobs = sqlx::query_as(
            "SELECT a.file_path, p.status FROM preview_jobs p JOIN assets a ON a.id = p.asset_id
             ORDER BY a.file_path",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        (assets, jobs)
    }

    fn pairs(rows: &[(&str, &str)]) -> Vec<(String, String)> {
        rows.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn an_index_run_never_queues_a_group_member_other_than_its_main_item() {
        let (dir, db, lib) = temp_library().await;
        let [glb, fbx, obj, tree] = write_grouped_kit(dir.path());
        reindex(&db, &lib).await;
        assert_eq!(
            queued(&lib.pool).await,
            (
                pairs(&[
                    (fbx, "none"),
                    (glb, "pending"),
                    (obj, "none"),
                    (tree, "pending")
                ]),
                pairs(&[(glb, "pending"), (tree, "pending")]),
            )
        );
    }

    #[tokio::test]
    async fn a_packs_cover_images_are_queued_like_any_other_image() {
        let (dir, db, lib) = temp_library().await;
        let [glb, ..] = write_grouped_kit(dir.path());
        std::fs::write(dir.path().join("kit/License.txt"), "CC0").unwrap();
        write_png(&dir.path().join("kit/Preview.png"), 1024, 32);
        reindex(&db, &lib).await;
        let roles: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT file_path, support_role FROM assets WHERE support_role IS NOT NULL
             ORDER BY file_path",
        )
        .fetch_all(&lib.pool)
        .await
        .unwrap();
        assert_eq!(
            roles,
            [
                ("kit/License.txt".to_string(), Some("doc".to_string())),
                ("kit/Preview.png".to_string(), Some("cover".to_string())),
            ]
        );
        // The cover gets a job, the license takes no preview, and the members stay off
        let jobs = queued(&lib.pool).await.1;
        assert_eq!(
            jobs,
            pairs(&[
                (glb, "pending"),
                ("kit/Preview.png", "pending"),
                ("kit/tree.glb", "pending")
            ])
        );
        crate::preview::process_image_jobs(&lib, 2).await;
        let thumb: Option<String> = sqlx::query_scalar(
            "SELECT thumbnail_path FROM assets WHERE file_path = 'kit/Preview.png'",
        )
        .fetch_one(&lib.pool)
        .await
        .unwrap();
        assert!(thumb.is_some(), "the 1024 px cover got no thumbnail");
    }

    #[tokio::test]
    async fn new_files_wait_for_the_structure_pass_to_be_queued() {
        let (dir, db, lib) = temp_library().await;
        write_grouped_kit(dir.path());
        // The pass fails before it queues anything, so what is queued is what the walk queued
        sqlx::query("DROP TABLE folders")
            .execute(&lib.pool)
            .await
            .unwrap();
        reindex(&db, &lib).await;
        let (assets, jobs) = queued(&lib.pool).await;
        assert_eq!(jobs, []);
        assert!(
            assets.iter().all(|(_, status)| status == "none"),
            "{assets:?}"
        );
    }

    #[tokio::test]
    async fn a_datapackage_json_is_the_only_json_file_indexed_and_makes_a_pack() {
        let (dir, db, lib) = temp_library().await;
        std::fs::create_dir_all(dir.path().join("kit/Sub")).unwrap();
        for (path, text) in [
            ("kit/datapackage.json", r#"{"title": "City Pack"}"#),
            ("kit/Sub/DataPackage.JSON", r#"{"title": "Sub Pack"}"#),
            ("kit/other.json", "{}"),
            ("kit/rock.glb", "glTF"),
        ] {
            std::fs::write(dir.path().join(path), text).unwrap();
        }
        reindex(&db, &lib).await;

        let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT file_path, kind, preview_status, support_role FROM assets
             WHERE extension = 'json' ORDER BY file_path",
        )
        .fetch_all(&lib.pool)
        .await
        .unwrap();
        let doc = |path: &str| {
            let (kind, none, role) = ("document".into(), "none".into(), Some("doc".into()));
            (path.to_string(), kind, none, role)
        };
        assert_eq!(
            rows,
            [doc("kit/Sub/DataPackage.JSON"), doc("kit/datapackage.json")]
        );
        assert_eq!(
            queued(&lib.pool).await.1,
            pairs(&[("kit/rock.glb", "pending")])
        );
        let names: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT path, pack_json FROM folders WHERE is_pack ORDER BY path")
                .fetch_all(&lib.pool)
                .await
                .unwrap();
        let names: Vec<(&str, String)> = names
            .iter()
            .map(|(path, json)| {
                let pack = crate::structure::parse_pack(json.as_deref()).unwrap();
                (path.as_str(), pack.name)
            })
            .collect();
        assert_eq!(
            names,
            [("kit", "City Pack".into()), ("kit/Sub", "Sub Pack".into())]
        );
    }

    /// Writes a `width` x `height` PNG to `path`. Large ones are short, so the decode stays cheap.
    fn write_png(path: &Path, width: u32, height: u32) {
        image::RgbImage::new(width, height).save(path).unwrap();
    }

    /// A library rooted at a fresh temp dir, open in a fresh database.
    async fn temp_library() -> (tempfile::TempDir, Database, LibraryHandle) {
        // Not `tempfile::tempdir()`: the walker skips hidden dirs, and that names it `.tmpXXXX`.
        let dir = tempfile::Builder::new()
            .prefix("opendam-lib-")
            .tempdir()
            .unwrap();
        std::fs::create_dir_all(dir.path().join(".opendam")).unwrap();
        let lib = LibraryHandle {
            id: "lib".into(),
            name: "lib".into(),
            root_path: dir.path().to_path_buf(),
            pool: memory_library_pool().await,
        };
        let db = memory_database(vec![lib.clone()]).await;
        (dir, db, lib)
    }

    async fn reindex(db: &Database, lib: &LibraryHandle) {
        index_paths(db, &lib.id, &[lib.root_path.to_string_lossy().to_string()])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_pack_edit_survives_a_reindex_and_its_folder_going_and_coming_back() {
        use crate::commands::folder::{set_pack_override_in, PackEdit};
        let (dir, db, lib) = temp_library().await;
        let kit = dir.path().join("kits/Rock Kit");
        let extract = || {
            std::fs::create_dir_all(&kit).unwrap();
            let license = "Rock Kit (1.0)\n\nCreated/distributed by Kenney (www.kenney.nl)\n";
            std::fs::write(kit.join("License.txt"), license).unwrap();
            std::fs::write(kit.join("rock.glb"), "glTF").unwrap();
        };
        let name = || async {
            let name: Option<String> = sqlx::query_scalar(
                "SELECT json_extract(pack_json, '$.name') FROM folders WHERE path = 'kits/Rock Kit'",
            )
            .fetch_optional(&lib.pool)
            .await
            .unwrap();
            name
        };
        extract();
        reindex(&db, &lib).await;
        assert_eq!(name().await.as_deref(), Some("Rock Kit"));
        let edit = PackEdit::Details {
            name: Some("My rocks".into()),
            license: None,
            source_url: None,
            origin: None,
            normal_convention: None,
        };
        set_pack_override_in(&db, "lib", "kits/Rock Kit", edit)
            .await
            .unwrap();
        assert_eq!(name().await.as_deref(), Some("My rocks"));

        reindex(&db, &lib).await;
        assert_eq!(name().await.as_deref(), Some("My rocks"));
        // An update extracted over the library removes the folder, then brings it back.
        std::fs::remove_dir_all(&kit).unwrap();
        reindex(&db, &lib).await;
        assert_eq!(name().await, None);
        extract();
        reindex(&db, &lib).await;
        assert_eq!(name().await.as_deref(), Some("My rocks"));
    }

    /// The asset's (thumbnail_path, preview_status) and its active preview job count.
    async fn preview_state(pool: &Pool<Sqlite>) -> (Option<String>, String, i64) {
        let (thumb, status): (Option<String>, String) =
            sqlx::query_as("SELECT thumbnail_path, preview_status FROM assets")
                .fetch_one(pool)
                .await
                .unwrap();
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM preview_jobs WHERE status IN ('pending', 'queued', 'processing', 'fallback')",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        (thumb, status, active)
    }

    #[tokio::test]
    async fn small_png_that_grows_past_512_gets_a_thumbnail() {
        let (dir, db, lib) = temp_library().await;
        let source = dir.path().join("sprite.png");
        write_png(&source, 64, 64);
        reindex(&db, &lib).await;
        crate::preview::process_image_jobs(&lib, 2).await;
        assert_eq!(preview_state(&lib.pool).await, (None, "done".into(), 0));

        write_png(&source, 1024, 32);
        reindex(&db, &lib).await;
        assert_eq!(preview_state(&lib.pool).await, (None, "pending".into(), 1));

        crate::preview::process_image_jobs(&lib, 2).await;
        let (thumb, status, active) = preview_state(&lib.pool).await;
        let thumb = thumb.expect("a thumbnail once the source is larger than 512 px");
        assert_eq!((status.as_str(), active), ("done", 0));
        assert_eq!(
            image::image_dimensions(dir.path().join(&thumb)).unwrap(),
            (512, 16)
        );
    }

    #[tokio::test]
    async fn changed_raster_with_an_active_job_gets_no_second_one() {
        let (dir, db, lib) = temp_library().await;
        let source = dir.path().join("sprite.png");
        write_png(&source, 64, 64);
        reindex(&db, &lib).await;
        assert_eq!(preview_state(&lib.pool).await.2, 1);

        write_png(&source, 1024, 32);
        reindex(&db, &lib).await;

        assert_eq!(preview_state(&lib.pool).await, (None, "pending".into(), 1));
    }

    #[tokio::test]
    async fn raster_that_changes_while_its_job_runs_gets_a_second_job() {
        let (dir, db, lib) = temp_library().await;
        let source = dir.path().join("sprite.png");
        write_png(&source, 64, 64);
        reindex(&db, &lib).await;
        // The image queue has claimed the job and is decoding the 64 px file.
        sqlx::query("UPDATE preview_jobs SET status = 'processing'")
            .execute(&lib.pool)
            .await
            .unwrap();

        write_png(&source, 1024, 32);
        reindex(&db, &lib).await;
        let pending: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM preview_jobs WHERE status = 'pending'")
                .fetch_one(&lib.pool)
                .await
                .unwrap();
        assert_eq!(pending, 1);

        // The claimed job finishes from the old pixels: no thumbnail, and the row says done.
        sqlx::query("UPDATE preview_jobs SET status = 'done' WHERE status = 'processing'")
            .execute(&lib.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE assets SET preview_status = 'done', thumbnail_path = NULL")
            .execute(&lib.pool)
            .await
            .unwrap();
        crate::preview::process_image_jobs(&lib, 2).await;

        let (thumb, status, active) = preview_state(&lib.pool).await;
        let thumb = thumb.expect("the second job thumbnails the 1024 px file");
        assert_eq!((status.as_str(), active), ("done", 0));
        assert_eq!(
            image::image_dimensions(dir.path().join(&thumb)).unwrap(),
            (512, 16)
        );
    }

    #[tokio::test]
    async fn changed_model_keeps_its_preview() {
        let (dir, db, lib) = temp_library().await;
        let source = dir.path().join("ship.glb");
        std::fs::write(&source, "v1").unwrap();
        reindex(&db, &lib).await;
        sqlx::query("UPDATE preview_jobs SET status = 'done'")
            .execute(&lib.pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE assets SET preview_status = 'done', thumbnail_path = '.opendam/thumbs/s.png'",
        )
        .execute(&lib.pool)
        .await
        .unwrap();

        std::fs::write(&source, "version 2").unwrap();
        reindex(&db, &lib).await;

        assert_eq!(
            preview_state(&lib.pool).await,
            (Some(".opendam/thumbs/s.png".into()), "done".into(), 0)
        );
    }

    // ── The advisory lock ───────────────────────────────────────────────────

    /// A lock env on "this-machine" as process 100, with a clock the test moves and counts reads
    /// of: after acquiring, only heartbeats read it.
    struct TestEnv {
        secs: Arc<std::sync::atomic::AtomicU64>,
        reads: Arc<std::sync::atomic::AtomicU64>,
        env: LockEnv,
    }

    impl TestEnv {
        fn new(alive: impl Fn(u32, u64) -> bool + Send + Sync + 'static) -> Self {
            let secs = Arc::new(std::sync::atomic::AtomicU64::new(1_000_000));
            let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let now = {
                let (secs, reads) = (secs.clone(), reads.clone());
                move || {
                    reads.fetch_add(1, Ordering::SeqCst);
                    UNIX_EPOCH + Duration::from_secs(secs.load(Ordering::SeqCst))
                }
            };
            let env = LockEnv {
                now: Arc::new(now),
                alive: Arc::new(alive),
                machine: Some("this-machine".into()),
                pid: 100,
                started: 5,
                settle: Duration::ZERO,
                heartbeat: Duration::from_secs(3600),
            };
            TestEnv { secs, reads, env }
        }

        fn now(&self) -> SystemTime {
            UNIX_EPOCH + Duration::from_secs(self.secs.load(Ordering::SeqCst))
        }

        fn advance(&self, secs: u64) {
            self.secs.fetch_add(secs, Ordering::SeqCst);
        }

        /// Another app instance on this machine, process 200, sharing the clock.
        fn other(&self) -> LockEnv {
            LockEnv {
                pid: 200,
                ..self.env.clone()
            }
        }
    }

    async fn acquire(dir: &Path, env: &LockEnv) -> Option<IndexLock> {
        IndexLock::try_acquire_with(&dir.join("index.lock"), STALE_AFTER, env).await
    }

    fn write_lock_file(dir: &Path, token: &str, mtime: SystemTime) {
        let file = std::fs::File::create(dir.join("index.lock")).unwrap();
        (&file)
            .write_all(format!("{token}\n0\n").as_bytes())
            .unwrap();
        file.set_modified(mtime).unwrap();
    }

    fn lock_token(dir: &Path) -> Option<String> {
        read_token(&dir.join("index.lock")).ok()
    }

    fn held(lock: &IndexLock) -> &HeldLock {
        lock.held.as_deref().unwrap()
    }

    #[tokio::test]
    async fn acquiring_writes_a_token_naming_this_process_and_the_time() {
        let dir = tempfile::tempdir().unwrap();
        let t = TestEnv::new(|_, _| true);

        let lock = acquire(dir.path(), &t.env).await.unwrap();

        let content = std::fs::read_to_string(dir.path().join("index.lock")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines[1], "1000000");
        let parts: Vec<&str> = lines[0].split(' ').collect();
        assert_eq!(parts[..3], ["this-machine", "100", "5"]);
        assert!(Uuid::parse_str(parts[3]).is_ok(), "{content}");
        let mtime = std::fs::metadata(dir.path().join("index.lock")).unwrap();
        assert_eq!(mtime.modified().unwrap(), t.now());
        assert!(acquire(dir.path(), &t.other()).await.is_none());
        assert_eq!(lock_token(dir.path()).unwrap(), held(&lock).token);
    }

    #[tokio::test]
    async fn a_holder_whose_token_was_overwritten_while_settling_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let t = TestEnv::new(|_, _| true);
        let env = LockEnv {
            settle: Duration::from_millis(300),
            ..t.env.clone()
        };
        let path = dir.path().to_path_buf();
        let contender = tokio::spawn(async move { acquire(&path, &env).await.is_some() });

        // Another instance writes its token between this one's write and its re-read.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while lock_token(dir.path()).is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        write_lock_file(dir.path(), "other-machine 7 7 theirs", t.now());

        assert!(!contender.await.unwrap());
        assert_eq!(lock_token(dir.path()).unwrap(), "other-machine 7 7 theirs");
    }

    #[tokio::test]
    async fn a_heartbeat_keeps_a_hold_longer_than_120_s_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let t = TestEnv::new(|_, _| true);
        let lock = acquire(dir.path(), &t.env).await.unwrap();

        for _ in 0..5 {
            t.advance(30);
            held(&lock).beat(&t.env);
        }

        // Held for 150 s, beating every 30 s: another instance still yields.
        assert!(acquire(dir.path(), &t.other()).await.is_none());
        // Without beats the lock goes stale 120 s after the last one.
        t.advance(120);
        assert!(acquire(dir.path(), &t.other()).await.is_some());
    }

    #[tokio::test]
    async fn the_heartbeat_task_refreshes_the_lock_until_the_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        let t = TestEnv::new(|_, _| true);
        let env = LockEnv {
            heartbeat: Duration::from_millis(20),
            ..t.env.clone()
        };
        let lock = acquire(dir.path(), &env).await.unwrap();
        t.advance(500);

        let mtime = || {
            std::fs::metadata(dir.path().join("index.lock"))
                .and_then(|m| m.modified())
                .ok()
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while mtime() != Some(t.now()) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(mtime(), Some(t.now()));

        let token = held(&lock).token.clone();
        drop(lock);
        assert_eq!(mtime(), None);

        // A file holding the dropped guard's own token again: a heartbeat still running would
        // rewrite it, reading the clock.
        let old = t.now() - Duration::from_secs(1000);
        write_lock_file(dir.path(), &token, old);
        let reads = t.reads.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(mtime(), Some(old), "the heartbeat still runs");
        assert_eq!(t.reads.load(Ordering::SeqCst), reads);
    }

    #[tokio::test]
    async fn a_lock_naming_a_gone_process_on_this_machine_is_stale_at_once() {
        let dir = tempfile::tempdir().unwrap();
        // Process 4242 started at 7 is gone; every other process is alive.
        let t = TestEnv::new(|pid, started| (pid, started) != (4242, 7));

        // A fresh lock of a gone process on this machine is taken.
        write_lock_file(dir.path(), "this-machine 4242 7 dead", t.now());
        let lock = acquire(dir.path(), &t.env).await;
        assert!(lock.is_some());
        drop(lock);

        // A live one on this machine is not.
        write_lock_file(dir.path(), "this-machine 4242 8 live", t.now());
        assert!(acquire(dir.path(), &t.env).await.is_none());

        // Another machine's lock is judged by its mtime alone.
        write_lock_file(dir.path(), "other-machine 4242 7 theirs", t.now());
        assert!(acquire(dir.path(), &t.env).await.is_none());
        let old = t.now() - STALE_AFTER - Duration::from_secs(1);
        write_lock_file(dir.path(), "other-machine 4242 7 theirs", old);
        assert!(acquire(dir.path(), &t.env).await.is_some());
    }

    #[test]
    fn a_process_is_alive_only_with_its_pid_and_start_time() {
        let alive = LockEnv::system().alive;
        let pid = std::process::id();
        let started = process_start_time(pid).unwrap();

        assert!(alive(pid, started));
        assert!(!alive(pid, started + 1));
        assert!(!alive(0, started));
    }

    #[tokio::test]
    async fn dropping_a_guard_deletes_the_file_only_while_it_holds_its_token() {
        let dir = tempfile::tempdir().unwrap();
        let t = TestEnv::new(|_, _| true);

        drop(acquire(dir.path(), &t.env).await.unwrap());
        assert_eq!(lock_token(dir.path()), None);

        let lock = acquire(dir.path(), &t.env).await.unwrap();
        write_lock_file(dir.path(), "other-machine 7 7 theirs", t.now());
        drop(lock);
        assert_eq!(lock_token(dir.path()).unwrap(), "other-machine 7 7 theirs");
    }

    #[tokio::test]
    async fn a_heartbeat_that_finds_another_token_marks_the_guard_lost() {
        let dir = tempfile::tempdir().unwrap();
        let t = TestEnv::new(|_, _| true);
        let lock = acquire(dir.path(), &t.env).await.unwrap();

        held(&lock).beat(&t.env);
        assert!(!lock.is_lost());

        write_lock_file(dir.path(), "other-machine 7 7 theirs", t.now());
        held(&lock).beat(&t.env);
        assert!(lock.is_lost());
        assert_eq!(lock_token(dir.path()).unwrap(), "other-machine 7 7 theirs");

        // A deleted file is lost too.
        let lock = {
            std::fs::remove_file(dir.path().join("index.lock")).unwrap();
            acquire(dir.path(), &t.env).await.unwrap()
        };
        std::fs::remove_file(dir.path().join("index.lock")).unwrap();
        held(&lock).beat(&t.env);
        assert!(lock.is_lost());
        assert_eq!(lock_token(dir.path()), None);
    }

    #[tokio::test]
    async fn a_heartbeat_that_cant_read_the_lock_keeps_the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let t = TestEnv::new(|_, _| true);
        let lock = acquire(dir.path(), &t.env).await.unwrap();

        // A directory in the file's place fails the read with an error other than NotFound,
        // as a share's hiccup might: the next beat reads it again.
        std::fs::remove_file(dir.path().join("index.lock")).unwrap();
        std::fs::create_dir(dir.path().join("index.lock")).unwrap();
        held(&lock).beat(&t.env);

        assert!(!lock.is_lost());
    }

    #[tokio::test]
    async fn a_reread_waits_for_a_heartbeat_rewriting_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let t = TestEnv::new(|_, _| true);
        let lock = acquire(dir.path(), &t.env).await.unwrap();
        let path = dir.path().join("index.lock");
        let token = held(&lock).token.clone();

        std::thread::scope(|scope| {
            // A beat midway through its rewrite: it holds the mutex, and the file is empty.
            let beat = held(&lock).released.lock().unwrap();
            std::fs::write(&path, "").unwrap();
            let reread = scope.spawn(|| lock.still_held());
            std::thread::sleep(Duration::from_millis(100));
            assert!(
                !reread.is_finished(),
                "the re-read didn't wait for the beat"
            );
            std::fs::write(&path, format!("{token}\n0\n")).unwrap();
            drop(beat);

            assert!(reread.join().unwrap());
        });
        assert!(!lock.is_lost());
    }

    #[tokio::test]
    async fn a_lock_file_that_cant_be_written_lets_the_run_go_on_unlocked() {
        let missing = missing_root();
        let t = TestEnv::new(|_, _| true);

        let lock = acquire(&missing, &t.env).await.unwrap();

        assert!(lock.held.is_none() && lock.heartbeat.is_none());
        assert!(!lock.is_lost());
        drop(lock);
        assert!(!missing.exists());
    }

    #[tokio::test]
    async fn an_index_run_whose_lock_was_lost_prunes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let t = TestEnv::new(|_, _| true);
        let lib = library_with_one_asset("lib", dir.path().to_path_buf()).await;
        let ids = ["a1".to_string()];

        // A heartbeat found another token.
        let lock = acquire(dir.path(), &t.env).await.unwrap();
        write_lock_file(dir.path(), "other-machine 7 7 theirs", t.now());
        held(&lock).beat(&t.env);
        prune_missing(&lib.pool, &ids, Some(&lock)).await;
        assert_eq!(asset_count(&lib.pool).await, 1);
        drop(lock);

        // Another token since the last heartbeat (say the holder was suspended past 120 s): the
        // prune re-reads the file, and marks the guard lost.
        std::fs::remove_file(dir.path().join("index.lock")).unwrap();
        let lock = acquire(dir.path(), &t.env).await.unwrap();
        write_lock_file(dir.path(), "other-machine 7 7 theirs", t.now());
        prune_missing(&lib.pool, &ids, Some(&lock)).await;
        assert_eq!(asset_count(&lib.pool).await, 1);
        assert!(lock.is_lost());
        drop(lock);

        // A file it can't read now prunes nothing either, but isn't lost.
        std::fs::remove_file(dir.path().join("index.lock")).unwrap();
        let lock = acquire(dir.path(), &t.env).await.unwrap();
        std::fs::remove_file(dir.path().join("index.lock")).unwrap();
        std::fs::create_dir(dir.path().join("index.lock")).unwrap();
        prune_missing(&lib.pool, &ids, Some(&lock)).await;
        assert_eq!(asset_count(&lib.pool).await, 1);
        assert!(!lock.is_lost());
        drop(lock);

        // A held lock prunes.
        std::fs::remove_dir(dir.path().join("index.lock")).unwrap();
        let lock = acquire(dir.path(), &t.env).await.unwrap();
        prune_missing(&lib.pool, &ids, Some(&lock)).await;
        assert_eq!(asset_count(&lib.pool).await, 0);
    }

    #[test]
    fn the_lock_names_this_machine_boot_and_pid_namespace_by_a_keyed_hash() {
        let id = local_id_of("m", "b", "pid:[1]");

        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        // A clone with the same machine id but another boot, or a sandbox on the same boot with
        // its own pids, isn't this space.
        assert_ne!(id, local_id_of("m", "b2", "pid:[1]"));
        assert_ne!(id, local_id_of("m", "b", "pid:[2]"));
        assert_ne!(id, local_id_of("m2", "b", "pid:[1]"));

        let Some(here) = local_id() else { return };
        let raw = std::fs::read_to_string("/etc/machine-id")
            .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
            .unwrap();
        assert_eq!(here.len(), 32);
        assert_ne!(here, raw.trim());
    }

    // ── A scoped index, `Prune` and the locked variants ──────────────────

    /// Seeds a 1-byte asset row at `path` with no file behind it, tagged "keep".
    async fn seed_tagged(pool: &Pool<Sqlite>, path: &str) {
        sqlx::query("INSERT OR IGNORE INTO tags (id, name) VALUES ('t1', 'keep')")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
             VALUES (?, ?, 'x', 'fbx', 'model', 1, 0, 0)",
        )
        .bind(path)
        .bind(path)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES (?, 't1')")
            .bind(path)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn paths_of(pool: &Pool<Sqlite>, sql: &str) -> Vec<String> {
        sqlx::query_scalar(sql).fetch_all(pool).await.unwrap()
    }

    /// `temp_library` on a pool that enforces foreign keys, so a pruned row takes its tags along.
    async fn temp_library_enforcing() -> (tempfile::TempDir, Database, LibraryHandle) {
        let (dir, _, mut lib) = temp_library().await;
        lib.pool = crate::db::memory_library_pool_enforcing_foreign_keys().await;
        let db = memory_database(vec![lib.clone()]).await;
        (dir, db, lib)
    }

    #[tokio::test]
    async fn a_run_over_a_pack_reads_only_rows_under_it_on_whole_components() {
        let pool = memory_library_pool().await;
        for path in [
            "Pub/Pack",
            "Pub/Pack/Rock.fbx",
            "Pub/Pack/Sub/Tree.fbx",
            "Pub/Package/Rock.fbx",
            "Pub/Pack.fbx",
            "Pub/Pack0/Rock.fbx",
            "Pub/pack/Rock.fbx",
            "Other/Rock.fbx",
        ] {
            seed_tagged(&pool, path).await;
        }
        let root = Path::new("/lib");
        let read = |paths: &'static [&'static str]| {
            let pool = pool.clone();
            async move {
                let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
                let mut rows: Vec<String> = rows_in_scope(&pool, root, &paths)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|row| row.1)
                    .collect();
                rows.sort();
                rows
            }
        };

        assert_eq!(
            read(&["/lib/Pub/Pack"]).await,
            ["Pub/Pack", "Pub/Pack/Rock.fbx", "Pub/Pack/Sub/Tree.fbx"]
        );
        assert_eq!(
            read(&["/lib/Pub/Pack/", "/lib/Other"]).await,
            [
                "Other/Rock.fbx",
                "Pub/Pack",
                "Pub/Pack/Rock.fbx",
                "Pub/Pack/Sub/Tree.fbx"
            ]
        );
        assert_eq!(read(&["/lib"]).await.len(), 8, "the root covers every row");
        assert_eq!(read(&["/lib/Pub/Pack", "/lib/"]).await.len(), 8);
        assert!(read(&["/elsewhere/Pub/Pack"]).await.is_empty());
    }

    #[tokio::test]
    async fn a_run_over_a_pack_prunes_only_the_pack() {
        let (dir, db, lib) = temp_library_enforcing().await;
        std::fs::create_dir_all(dir.path().join("Pub/Pack")).unwrap();
        std::fs::write(dir.path().join("Pub/Pack/Rock.txt"), "rock").unwrap();
        for path in [
            "Pub/Pack/Gone.fbx",
            "Pub/Package/Rock.fbx",
            "Other/Tree.fbx",
        ] {
            seed_tagged(&lib.pool, path).await;
        }

        let pack = dir.path().join("Pub/Pack").to_string_lossy().to_string();
        index_paths(&db, &lib.id, &[pack]).await.unwrap();

        assert_eq!(
            paths_of(&lib.pool, "SELECT file_path FROM assets ORDER BY file_path").await,
            [
                "Other/Tree.fbx",
                "Pub/Pack/Rock.txt",
                "Pub/Package/Rock.fbx"
            ]
        );
        assert_eq!(
            paths_of(
                &lib.pool,
                "SELECT asset_id FROM asset_tags ORDER BY asset_id"
            )
            .await,
            ["Other/Tree.fbx", "Pub/Package/Rock.fbx"],
            "the rows outside the pack keep their tags"
        );

        // A run over the library root covers every row, as it always has.
        reindex(&db, &lib).await;
        assert_eq!(
            paths_of(&lib.pool, "SELECT file_path FROM assets").await,
            ["Pub/Pack/Rock.txt"]
        );
    }

    #[tokio::test]
    async fn the_lock_free_core_indexes_a_pool_no_database_holds() {
        let dir = tempfile::Builder::new()
            .prefix("opendam-lib-")
            .tempdir()
            .unwrap();
        std::fs::create_dir_all(dir.path().join("kits/rocks")).unwrap();
        std::fs::write(dir.path().join("kits/rocks/rock.glb"), "glTF").unwrap();
        let pool = memory_library_pool().await;

        let root = dir.path().to_string_lossy().to_string();
        index_pool(&pool, dir.path(), &[root], Prune::Now, None)
            .await
            .unwrap();

        assert_eq!(
            crate::structure::tests::derived_rows(&pool).await,
            crate::structure::tests::fresh_pass(&["kits/rocks/rock.glb"]).await,
            "the run indexed the file and ended with the structure pass"
        );
    }

    /// Writes each reported row to `journal`, through the inserting transaction, or fails.
    struct TestJournal {
        fail: bool,
    }

    impl InsertJournal for TestJournal {
        fn record<'a>(
            &'a self,
            tx: &'a mut SqliteConnection,
            inserted: &'a [(&'a str, &'a str)],
        ) -> BoxFuture<'a, sqlx::Result<()>> {
            Box::pin(async move {
                if self.fail {
                    return Err(sqlx::Error::Protocol("journal failed".into()));
                }
                for (id, path) in inserted {
                    sqlx::query("INSERT INTO journal (asset_id, file_path) VALUES (?, ?)")
                        .bind(id)
                        .bind(path)
                        .execute(&mut *tx)
                        .await?;
                }
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn a_deferred_run_prunes_nothing_and_journals_each_insert_in_its_transaction() {
        let (dir, db, lib) = temp_library().await;
        sqlx::query("CREATE TABLE journal (asset_id TEXT, file_path TEXT)")
            .execute(&lib.pool)
            .await
            .unwrap();
        std::fs::create_dir_all(dir.path().join("Pub/Pack")).unwrap();
        std::fs::write(dir.path().join("Pub/Pack/Kept.txt"), "kept").unwrap();
        reindex(&db, &lib).await;
        let folders = crate::structure::tests::derived_rows(&lib.pool)
            .await
            .folders;
        seed_tagged(&lib.pool, "Pub/Pack/Previous.fbx").await;
        std::fs::write(dir.path().join("Pub/Pack/Kept.txt"), "kept, changed").unwrap();
        std::fs::write(dir.path().join("Pub/Pack/New.txt"), "new").unwrap();
        std::fs::write(dir.path().join("Pub/Pack/Newer.txt"), "newer").unwrap();
        let pack = [dir.path().join("Pub/Pack").to_string_lossy().to_string()];
        let lock = db.index_lock(&lib.id);
        let index_guard = lock.lock().await;
        let advisory = IndexLock::try_acquire(dir.path()).await.unwrap();

        let failing = TestJournal { fail: true };
        let failed = index_paths_locked(
            &db,
            &lib.id,
            &pack,
            Prune::Defer(&failing),
            &index_guard,
            &advisory,
        )
        .await;
        assert!(failed.is_err());
        assert_eq!(
            paths_of(&lib.pool, "SELECT file_path FROM assets ORDER BY file_path").await,
            ["Pub/Pack/Kept.txt", "Pub/Pack/Previous.fbx"],
            "a failed journal rolls back the inserts it was told of"
        );

        let journal = TestJournal { fail: false };
        index_paths_locked(
            &db,
            &lib.id,
            &pack,
            Prune::Defer(&journal),
            &index_guard,
            &advisory,
        )
        .await
        .unwrap();

        assert_eq!(
            paths_of(&lib.pool, "SELECT file_path FROM assets ORDER BY file_path").await,
            [
                "Pub/Pack/Kept.txt",
                "Pub/Pack/New.txt",
                "Pub/Pack/Newer.txt",
                "Pub/Pack/Previous.fbx"
            ],
            "the row of a file no longer on disk stays"
        );
        assert_eq!(
            paths_of(
                &lib.pool,
                "SELECT j.file_path FROM journal j JOIN assets a ON a.id = j.asset_id
                 AND a.file_path = j.file_path ORDER BY j.file_path"
            )
            .await,
            ["Pub/Pack/New.txt", "Pub/Pack/Newer.txt"],
            "only inserted rows are journaled, an updated one is not"
        );
        assert_eq!(
            crate::structure::tests::derived_rows(&lib.pool)
                .await
                .folders,
            folders,
            "no structure pass ran: the pack still counts one item"
        );
    }

    #[tokio::test]
    async fn a_locked_index_and_structure_pass_never_lock_again() {
        let (dir, db, lib) = temp_library().await;
        std::fs::create_dir_all(dir.path().join("kits/rocks")).unwrap();
        std::fs::write(dir.path().join("kits/rocks/rock.glb"), "glTF").unwrap();
        let root = [dir.path().to_string_lossy().to_string()];
        let lock = db.index_lock(&lib.id);
        let index_guard = lock.lock().await;
        let advisory = IndexLock::try_acquire(dir.path()).await.unwrap();

        tokio::time::timeout(Duration::from_secs(10), async {
            index_paths_locked(&db, &lib.id, &root, Prune::Now, &index_guard, &advisory)
                .await
                .unwrap();
            crate::structure::rebuild_library_locked(&db, &lib, &index_guard, &advisory).await;
        })
        .await
        .expect("a locked variant waited for a lock its caller holds");

        // It indexed rather than yielding to its own caller's advisory lock.
        assert_eq!(
            crate::structure::tests::derived_rows(&lib.pool).await,
            crate::structure::tests::fresh_pass(&["kits/rocks/rock.glb"]).await
        );
    }
}
