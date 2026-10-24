use crate::commands::asset::{pictured, MAIN_OR_SINGLE};
use crate::db::{make_relative_path, resolve_path, LibraryHandle};
use crate::AppState;
use anyhow::{anyhow, Result};
use image::imageops::FilterType;
use serde_json::json;
use sqlx::{Pool, Sqlite};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tauri::Emitter;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

pub const THUMB_SIZE: u32 = 512;

/// Checks if an extension is a standard raster image that Rust can decode directly.
pub fn is_native_image(ext: &str) -> bool {
    matches!(
        ext.to_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "bmp" | "gif" | "tif" | "tiff" | "tga" | "webp"
    )
}

/// Whether a file with extension `ext` gets a preview job: models, layered and HDR images, and
/// every raster `is_native_image` accepts.
pub fn needs_preview(ext: &str) -> bool {
    matches!(
        ext.to_lowercase().as_str(),
        "max"
            | "fbx"
            | "obj"
            | "gltf"
            | "glb"
            | "dae"
            | "stl"
            | "3ds"
            | "skp"
            | "rfa"
            | "rvt"
            | "dwg"
            | "3dm"
            | "vrmat"
            | "psd"
            | "psb"
            | "ai"
            | "exr"
            | "hdr"
    ) || is_native_image(ext)
}

/// Checks if an extension is a raster image the webview displays itself, so a small one needs
/// no thumbnail.
pub fn is_webview_displayable(ext: &str) -> bool {
    matches!(
        ext.to_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "bmp" | "gif" | "webp"
    )
}

/// Writes `asset_id`'s thumbnail for `source` into `thumb_dir` and returns its file name, or
/// `None` when the original is shown instead. Thumbnails are never upscaled: a source
/// `THUMB_SIZE` px or smaller gets no thumbnail if the webview can display it, and otherwise
/// (TGA, TIFF) a PNG at its own size. Larger sources are downsampled with Triangle (bilinear)
/// filtering, 4x-8x faster than Lanczos3.
pub fn generate_image_thumb(
    source: &Path,
    thumb_dir: &Path,
    asset_id: &str,
) -> Result<Option<String>> {
    let ext = source
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    // The header alone gives the size, so a small displayable image is never decoded.
    let (width, height) = image::image_dimensions(source)?;
    let small = width.max(height) <= THUMB_SIZE;
    if small && is_webview_displayable(&ext) {
        return Ok(None);
    }

    let img = image::open(source)?;
    // Named by asset id: file names repeat across folders, asset ids don't.
    let (thumb, file_name) = if small {
        (img, format!("{}_thumb.png", asset_id))
    } else {
        let thumb_ext = if ext == "png" || ext == "webp" {
            "png"
        } else {
            "jpg"
        };
        (
            img.resize(THUMB_SIZE, THUMB_SIZE, FilterType::Triangle),
            format!("{}_thumb.{}", asset_id, thumb_ext),
        )
    };

    let _ = std::fs::create_dir_all(thumb_dir);
    let output = thumb_dir.join(&file_name);
    if file_name.ends_with(".jpg") {
        thumb
            .to_rgb8()
            .save_with_format(&output, image::ImageFormat::Jpeg)?;
    } else {
        thumb.save(&output)?;
    }
    Ok(Some(file_name))
}

/// Runs `decode`, turning a panic into `Err`: a malformed file can panic inside
/// the decoder, and that should fail one image, not the app. Relies on
/// `[profile.release]` unwinding on panic.
pub(crate) fn catch_decoder_panic<T>(
    decode: impl FnOnce() -> Result<T> + std::panic::UnwindSafe,
) -> Result<T> {
    std::panic::catch_unwind(decode).unwrap_or_else(|_| Err(anyhow!("image decoder panicked")))
}

/// Passes each line of `reader` to `log` until EOF. Bytes that aren't UTF-8,
/// such as raw output from a native extension, are decoded lossily so the pipe
/// keeps draining and the worker never writes into a closed pipe.
async fn forward_lines(reader: impl AsyncRead + Unpin, mut log: impl FnMut(&str)) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => return,
            Ok(_) => log(String::from_utf8_lossy(&buf).trim_end()),
            Err(e) => {
                tracing::warn!("Stopped reading Python worker stderr: {}", e);
                return;
            }
        }
    }
}

/// Persistent Python worker process communicating over JSON-RPC on stdin/stdout.
/// Stays alive across requests to eliminate Python cold-start overhead (200-500ms per file).
struct PythonWorker {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
}

impl PythonWorker {
    async fn spawn(py_exec: &Path, worker_py: &Path) -> Result<Self> {
        let mut cmd = Command::new(py_exec);
        cmd.arg(worker_py)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("PYTHONUNBUFFERED", "1")
            .env("PYTHONIOENCODING", "utf-8");

        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to take python stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to take python stdout"))?;
        let reader = BufReader::new(stdout);

        // Python tracebacks and library warnings land in the app log.
        if let Some(stderr) = child.stderr.take() {
            tauri::async_runtime::spawn(forward_lines(
                stderr,
                |line| tracing::warn!(target: "preview_worker", "{}", line),
            ));
        }

        Ok(Self {
            child,
            stdin,
            reader,
        })
    }

    async fn execute(&mut self, req: &serde_json::Value) -> Result<serde_json::Value> {
        let req_str = req.to_string() + "\n";
        self.stdin.write_all(req_str.as_bytes()).await?;
        self.stdin.flush().await?;

        // Only a JSON line carrying this request's own `id` is its reply, so a stray reply
        // can never be taken as another request's result.
        let mut line = String::new();
        loop {
            line.clear();
            let bytes_read = self.reader.read_line(&mut line).await?;
            if bytes_read == 0 {
                return Err(anyhow!("Python worker closed stdout unexpectedly"));
            }
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                continue;
            }
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if val.get("id") == req.get("id") {
                    return Ok(val);
                }
                tracing::warn!(
                    "Skipped a Python worker reply to another request: {}",
                    trimmed
                );
            }
        }
    }

    async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

/// Picks the Python interpreter and `worker.py` for the preview worker.
/// `OPENDAM_PYTHON`/`OPENDAM_WORKER` win. Otherwise the first preview engine
/// with a `worker.py` is used: the one `make install` puts under
/// `${XDG_DATA_HOME:-~/.local/share}/opendam/preview-engine`, then a dev
/// checkout's `packages/preview-engine` under one of `search_roots`. Its
/// `.venv` supplies Python; without one, `python3` on `PATH` does.
fn find_preview_worker(
    var: impl Fn(&str) -> Option<OsString>,
    search_roots: &[PathBuf],
) -> (PathBuf, PathBuf) {
    let data_home = var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".local").join("share")));
    let engine = data_home
        .map(|d| d.join("opendam").join("preview-engine"))
        .into_iter()
        .chain(
            search_roots
                .iter()
                .map(|r| r.join("packages").join("preview-engine")),
        )
        .find(|e| e.join("worker.py").is_file());

    let python = var("OPENDAM_PYTHON").map(PathBuf::from).unwrap_or_else(|| {
        engine
            .as_ref()
            .map(|e| e.join(".venv").join("bin").join("python"))
            .filter(|p| p.is_file())
            .or_else(|| {
                std::env::split_paths(&var("PATH")?)
                    .map(|dir| dir.join("python3"))
                    .find(|p| p.is_file())
            })
            .unwrap_or_else(|| PathBuf::from("python3"))
    });
    let worker = var("OPENDAM_WORKER")
        .map(PathBuf::from)
        .or_else(|| engine.map(|e| e.join("worker.py")))
        .unwrap_or_else(|| PathBuf::from("worker.py"));
    (python, worker)
}

/// Starts the preview queues. The Python queue's worker also serves `requests`, between jobs.
pub fn start_preview_worker(state: Arc<AppState>, requests: mpsc::Receiver<WorkerRequest>) {
    tauri::async_runtime::spawn(async move {
        // The dev checkout is found from the cwd, the exe dir and their ancestors.
        let current_dir = std::env::current_dir().unwrap_or_default();
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| current_dir.clone());
        let search_roots: Vec<PathBuf> = current_dir
            .ancestors()
            .take(5)
            .chain(exe_dir.ancestors().take(5))
            .map(Path::to_path_buf)
            .collect();

        let (py_exec, worker_py) = find_preview_worker(|k| std::env::var_os(k), &search_roots);
        let _ = state.preview_worker.set(worker_py.clone());

        tracing::info!(
            "Preview worker using Python: {:?}, Script: {:?}",
            py_exec,
            worker_py
        );

        // Crash-recovery: Reset any orphaned jobs left in 'processing' ONCE at startup
        for lib in state.db.get_all_libraries().await {
            let _ = sqlx::query(
                "UPDATE preview_jobs SET status = 'queued' WHERE status = 'processing'",
            )
            .execute(&lib.pool)
            .await;
        }

        // Images and Python-backed formats run on separate queues so a slow
        // 3D conversion never holds up the (much larger) image backlog.
        tauri::async_runtime::spawn(run_image_queue(state.clone()));
        run_python_queue(state, py_exec, worker_py, requests).await;
    });
}

/// SQL list of the extensions `is_native_image` accepts, matched against `assets.extension`.
pub(crate) const NATIVE_EXTS_SQL: &str =
    "('jpg','jpeg','png','bmp','gif','tif','tiff','tga','webp')";

/// A claimed job: (job_id, asset_id, file_path)
type Job = (String, String, String);

/// A group member whose main item has no preview of its own, as a `.blend` never does, and is
/// not pictured by a render that fits (`commands::asset::pictured`): the structure pass queues
/// one such member as the group's stand-in (`structure::apply_previews`), and its job is
/// claimed. Every other member's is skipped.
fn stands_in() -> String {
    format!(
        "EXISTS (SELECT 1 FROM assets g
             WHERE g.id = a.group_id AND g.preview_status = 'none' AND g.thumbnail_path IS NULL
               AND NOT {})",
        pictured("g")
    )
}

/// Whether the queue claims a job for asset `a` (`claim_jobs`): a main item or a file on its
/// own that isn't pictured, or its group's stand-in. The model preview pass requeues by it too
/// (`model_preview_regen`).
pub(crate) fn claimable() -> String {
    format!(
        "(({MAIN_OR_SINGLE} AND NOT {}) OR {})",
        pictured("a"),
        stands_in()
    )
}

/// The job states a worker can still claim a job from.
pub(crate) const WAITING_JOBS: &str = "('pending', 'queued', 'fallback')";
/// The job states of a job not yet finished: waiting, or running.
pub(crate) const ACTIVE_JOBS: &str = "('pending', 'queued', 'processing', 'fallback')";

/// The assets with a waiting job.
pub(crate) async fn assets_waiting_for_jobs(
    pool: &Pool<Sqlite>,
) -> sqlx::Result<std::collections::HashSet<String>> {
    let ids: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT DISTINCT asset_id FROM preview_jobs WHERE status IN {WAITING_JOBS}"
    ))
    .fetch_all(pool)
    .await?;
    Ok(ids.into_iter().collect())
}

/// Takes asset `id` off the queue: drops the jobs it is still waiting on and moves it from
/// `pending` back to `none`. A running job finishes, and a thumbnail it has stays.
pub(crate) async fn unqueue(conn: &mut sqlx::SqliteConnection, id: &str) -> sqlx::Result<()> {
    sqlx::query(&format!(
        "DELETE FROM preview_jobs WHERE asset_id = ? AND status IN {WAITING_JOBS}"
    ))
    .bind(id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE assets SET preview_status = 'none' WHERE id = ? AND preview_status = 'pending'",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Puts asset `id` on the queue: marks it `pending` and adds a job unless one is unfinished.
pub(crate) async fn enqueue(
    conn: &mut sqlx::SqliteConnection,
    id: &str,
    now: i64,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE assets SET preview_status = 'pending' WHERE id = ?")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(&format!(
        "INSERT INTO preview_jobs (id, asset_id, status, created_at)
         SELECT ?, ?, 'pending', ? WHERE NOT EXISTS (
             SELECT 1 FROM preview_jobs WHERE asset_id = ? AND status IN {ACTIVE_JOBS})"
    ))
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(id)
    .bind(now)
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Fetches up to `limit` jobs matching `filter` and marks them as 'processing'.
async fn claim_jobs(pool: &Pool<Sqlite>, filter: &str, limit: usize) -> Vec<Job> {
    let jobs: Vec<Job> = sqlx::query_as(&format!(
        // A group member other than its main item never shows in the grid, and an item whose
        // bundled preview fits a card shows that, so neither gets a preview (plan C15, narrowed
        // by small-bundled-previews C4). The structure pass drops their waiting jobs; this skips
        // any queued since.
        "SELECT p.id, p.asset_id, a.file_path FROM preview_jobs p JOIN assets a ON p.asset_id = a.id
         WHERE ({filter}) AND {} LIMIT {limit}",
        claimable()
    ))
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    let mut claimed = Vec::with_capacity(jobs.len());
    for job in jobs {
        match sqlx::query("UPDATE preview_jobs SET status = 'processing' WHERE id = ?")
            .bind(&job.0)
            .execute(pool)
            .await
        {
            Ok(_) => claimed.push(job),
            Err(e) => tracing::error!("Failed to update job status to processing: {}", e),
        }
    }
    claimed
}

fn resolve_source(root_path: &Path, file_path: &str) -> PathBuf {
    if Path::new(file_path).is_absolute() {
        PathBuf::from(file_path)
    } else {
        root_path.join(file_path)
    }
}

/// The pack holding the asset at `file_path`, relative to the library root: the innermost
/// folder marked as a pack that is the asset's own folder or one of its ancestors, or `None`
/// outside a pack (plan textured-fbx-previews, C1). Matched with `substr`, not `LIKE`, since
/// folder names hold `_` and `%`.
async fn pack_root(pool: &Pool<Sqlite>, file_path: &str) -> Option<String> {
    let folder = file_path.rsplit_once('/').map_or("", |(dir, _)| dir);
    sqlx::query_scalar(
        "SELECT path FROM folders
         WHERE is_pack AND (path = '' OR path = ?1 OR substr(?1, 1, length(path) + 1) = path || '/')
         ORDER BY length(path) DESC LIMIT 1",
    )
    .bind(folder)
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!("Failed to read the pack of {}: {}", file_path, e);
        None
    })
}

/// The worker's `model_meta` request for job `job_id`. `library_root` and, inside a pack,
/// `pack_root` bound where the worker may look for an FBX/3DS model's textures.
async fn model_meta_request(
    lib: &LibraryHandle,
    job_id: &str,
    asset_id: &str,
    file_path: &str,
) -> serde_json::Value {
    let root_path = &lib.root_path;
    let mut params = json!({
        "source": resolve_source(root_path, file_path).to_string_lossy(),
        "output_dir": root_path.join(".opendam").join("thumbs").to_string_lossy(),
        "name": asset_id,
        "library_root": root_path.to_string_lossy(),
    });
    if let Some(pack) = pack_root(&lib.pool, file_path).await {
        params["pack_root"] = json!(resolve_path(root_path, &pack));
    }
    json!({"id": job_id, "method": "model_meta", "params": params})
}

/// A finished preview job, as `preview:done` reports it. Every path is absolute; a `None`
/// `thumb_path` means the asset has no thumbnail. `file_path` is the asset's own file and
/// `previous_thumb_path` the thumbnail it had before the job, so a card showing either as a
/// model's bundled preview can switch to the new picture.
#[derive(Debug)]
pub(crate) struct PreviewDone {
    asset_id: String,
    file_path: Option<String>,
    previous_thumb_path: Option<String>,
    thumb_path: Option<String>,
    model_path: Option<String>,
}

fn emit_done(state: &AppState, done: &PreviewDone) {
    let _ = state.app_handle.emit(
        "preview:done",
        json!({
            "assetId": &done.asset_id,
            "filePath": &done.file_path,
            "previousThumbnailPath": &done.previous_thumb_path,
            "thumbnailPath": &done.thumb_path,
            "previewModel": &done.model_path,
        }),
    );
}

/// What the preview engine reports about a preview it made (plan textured-fbx-previews C7): its
/// `preview_version`, and for FBX/3DS its `preview_textures` summary as JSON. A result without
/// them, from an older engine or a Rust thumbnail, has neither, which reads as version 1.
#[derive(Debug, Default)]
pub(crate) struct EngineReport {
    version: Option<i64>,
    textures: Option<String>,
}

impl EngineReport {
    /// The report in a worker's `model_meta` result.
    pub(crate) fn of_result(result: &serde_json::Value) -> Self {
        EngineReport {
            version: result.get("preview_version").and_then(|v| v.as_i64()),
            textures: result
                .get("preview_textures")
                .filter(|v| !v.is_null())
                .map(|v| v.to_string()),
        }
    }
}

/// Marks a job done and stores the asset's preview paths, replacing any old ones: a `None`
/// `rel_thumb` clears `thumbnail_path`, and an old `_thumb` file the new path doesn't name is
/// deleted first. `rel_thumb` is relative to the library root; `model_path` is absolute. The
/// engine's `report` replaces the old one in `preview_version` and `preview_textures`, outside
/// the searched `metadata`.
pub(crate) async fn complete_job(
    pool: &Pool<Sqlite>,
    root_path: &Path,
    job_id: &str,
    asset_id: &str,
    rel_thumb: Option<String>,
    model_path: Option<String>,
    report: &EngineReport,
) -> PreviewDone {
    let _ = sqlx::query("UPDATE preview_jobs SET status = 'done' WHERE id = ?")
        .bind(job_id)
        .execute(pool)
        .await;

    let (file_path, previous_thumb): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT file_path, thumbnail_path FROM assets WHERE id = ?")
            .bind(asset_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .map_or((None, None), |(file, thumb): (String, Option<String>)| {
                (Some(file), thumb)
            });
    delete_replaced_thumb(pool, root_path, asset_id, rel_thumb.as_deref()).await;
    if let Some(ref model_path) = model_path {
        let rel_model = make_relative_path(root_path, Path::new(model_path));
        let _ = sqlx::query(
            "UPDATE assets SET preview_status = 'done', thumbnail_path = ?, metadata = json_set(coalesce(nullif(metadata, ''), '{}'), '$.preview_model', ?), preview_version = ?, preview_textures = ? WHERE id = ?"
        )
        .bind(&rel_thumb)
        .bind(&rel_model)
        .bind(report.version)
        .bind(&report.textures)
        .bind(asset_id)
        .execute(pool)
        .await;
    } else {
        let _ = sqlx::query(
            "UPDATE assets SET preview_status = 'done', thumbnail_path = ?, preview_version = ?, preview_textures = ? WHERE id = ?",
        )
        .bind(&rel_thumb)
        .bind(report.version)
        .bind(&report.textures)
        .bind(asset_id)
        .execute(pool)
        .await;
    }

    PreviewDone {
        asset_id: asset_id.to_string(),
        file_path: file_path.map(|f| resolve_path(root_path, &f)),
        previous_thumb_path: previous_thumb.map(|t| resolve_path(root_path, &t)),
        thumb_path: rel_thumb.as_ref().map(|r| resolve_path(root_path, r)),
        model_path,
    }
}

/// Deletes the asset's current `_thumb` file under `.opendam/thumbs` unless it is
/// `new_rel_thumb`, so a thumbnail that is cleared or renamed leaves no orphaned file. It runs
/// before the row changes: if the app stops in between, the row points at a missing file and
/// the UI falls back to the original. Other files there, such as a model's `_preview.glb`
/// stored as its thumbnail, may be the job's new output and are kept.
async fn delete_replaced_thumb(
    pool: &Pool<Sqlite>,
    root_path: &Path,
    asset_id: &str,
    new_rel_thumb: Option<&str>,
) {
    let old: Option<String> = sqlx::query_scalar("SELECT thumbnail_path FROM assets WHERE id = ?")
        .bind(asset_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten();
    let Some(old) = old.filter(|old| Some(old.as_str()) != new_rel_thumb) else {
        return;
    };
    if let Some(old_path) = owned_thumb_file(root_path, &old) {
        remove_thumb_file(&old_path).await;
    }
}

/// `rel_thumb` as an absolute path if it names a `_thumb` file under the library's
/// `.opendam/thumbs`, the only files a preview job or the thumbnail cleanup pass deletes.
pub(crate) fn owned_thumb_file(root_path: &Path, rel_thumb: &str) -> Option<PathBuf> {
    let path = PathBuf::from(resolve_path(root_path, rel_thumb));
    let is_thumb_file = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains("_thumb."));
    (is_thumb_file && path.starts_with(root_path.join(".opendam").join("thumbs"))).then_some(path)
}

/// Deletes a thumbnail file. One that is already gone is not an error.
pub(crate) async fn remove_thumb_file(path: &Path) {
    if let Err(e) = tokio::fs::remove_file(path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("Could not delete old thumbnail {:?}: {}", path, e);
        }
    }
}

/// A failed job: (job_id, asset_id)
type FailedJob = (String, String);

async fn fail_job(pool: &Pool<Sqlite>, job_id: &str, asset_id: &str) {
    tracing::error!("Job {} failed", job_id);
    let _ = sqlx::query("UPDATE preview_jobs SET status = 'error' WHERE id = ?")
        .bind(job_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("UPDATE assets SET preview_status = 'error' WHERE id = ?")
        .bind(asset_id)
        .execute(pool)
        .await;
}

fn emit_error(state: &AppState, (job_id, asset_id): &FailedJob) {
    let _ = state.app_handle.emit(
        "preview:error",
        json!({
            "assetId": asset_id,
            "jobId": job_id,
        }),
    );
}

/// Generates raster image thumbnails in Rust, one blocking task per CPU core.
/// Images the `image` crate can't decode are handed to the Python queue as 'fallback' jobs.
async fn run_image_queue(state: Arc<AppState>) {
    let parallelism = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    loop {
        let mut processed_any = false;

        for lib in state.db.get_all_libraries().await {
            let (claimed, done) = process_image_jobs(&lib, parallelism).await;
            processed_any |= claimed;
            for d in &done {
                emit_done(&state, d);
            }
        }

        if !processed_any {
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        }
    }
}

/// Claims and runs one round of `lib`'s image jobs. Returns whether any job was claimed, and the
/// jobs that finished, for `preview:done`.
pub(crate) async fn process_image_jobs(
    lib: &LibraryHandle,
    parallelism: usize,
) -> (bool, Vec<PreviewDone>) {
    let filter = format!(
        "p.status IN ('queued', 'pending') AND lower(coalesce(a.extension, '')) IN {}",
        NATIVE_EXTS_SQL
    );
    let thumb_dir = lib.root_path.join(".opendam").join("thumbs");

    let jobs = claim_jobs(&lib.pool, &filter, parallelism * 8).await;
    let claimed = !jobs.is_empty();

    let mut tasks = tokio::task::JoinSet::new();
    let mut results = Vec::new();
    for (job_id, asset_id, file_path) in jobs {
        let source = resolve_source(&lib.root_path, &file_path);
        let thumb_dir = thumb_dir.clone();

        tasks.spawn_blocking(move || {
            let res = catch_decoder_panic(|| generate_image_thumb(&source, &thumb_dir, &asset_id))
                .map_err(|e| format!("{}: {}", source.display(), e));
            (job_id, asset_id, res)
        });

        while tasks.len() >= parallelism {
            if let Some(Ok(r)) = tasks.join_next().await {
                results.push(r);
            }
        }
    }
    while let Some(r) = tasks.join_next().await {
        if let Ok(r) = r {
            results.push(r);
        }
    }

    let mut done = Vec::new();
    for (job_id, asset_id, res) in results {
        match res {
            Ok(thumb_file_name) => {
                let rel_thumb = thumb_file_name.map(|f| format!(".opendam/thumbs/{}", f));
                done.push(
                    complete_job(
                        &lib.pool,
                        &lib.root_path,
                        &job_id,
                        &asset_id,
                        rel_thumb,
                        None,
                        &EngineReport::default(),
                    )
                    .await,
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Native Rust image thumbnail failed for {}. Falling back to Python.",
                    e
                );
                let _ = sqlx::query("UPDATE preview_jobs SET status = 'fallback' WHERE id = ?")
                    .bind(&job_id)
                    .execute(&lib.pool)
                    .await;
            }
        }
    }
    (claimed, done)
}

/// How long a new worker process has to answer `engine_info`.
const ENGINE_INFO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a job has before its worker is killed.
const JOB_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the queue waits to start a worker again after `failures` starts in a row got no
/// answer to `engine_info`: no time after the first, then 10 s, 30 s, 60 s, then 5 minutes.
fn restart_delay(failures: u32) -> Duration {
    Duration::from_secs(match failures {
        0 | 1 => 0,
        2 => 10,
        3 => 30,
        4 => 60,
        _ => 300,
    })
}

/// The preview engine version a worker reported in its `engine_info` reply. An error reply,
/// such as an older worker's `Unknown method`, is version 1.
fn reported_version(reply: &serde_json::Value) -> i64 {
    reply
        .get("result")
        .and_then(|r| r.get("preview_version"))
        .and_then(|v| v.as_i64())
        .unwrap_or(1)
}

/// Whether a worker's `engine_info` reply says UnityPy imports, `None` when it doesn't say.
fn reported_unitypy(reply: &serde_json::Value) -> Option<bool> {
    reply.get("result")?.get("unitypy")?.as_bool()
}

/// The persistent Python worker that serves the queue, and the engine version it reported
/// (plan textured-fbx-previews C9, "Engine gate"). Each process it starts is first asked its
/// `engine_info`, and only a real reply publishes a version in `version`. Until then, and
/// whenever no process is serving, `version` holds `None`.
struct PreviewEngine {
    py_exec: PathBuf,
    worker_py: PathBuf,
    version: watch::Sender<Option<i64>>,
    /// Whether UnityPy imports, as the last process to answer `engine_info` said, kept while
    /// no process serves.
    unitypy: watch::Sender<Option<bool>>,
    info_timeout: Duration,
    worker: Option<PythonWorker>,
    /// Processes started so far.
    starts: u32,
    /// Starts in a row that got no reply to `engine_info`.
    failures: u32,
    /// When the next start may run, after a start that got no reply.
    retry_at: Option<Instant>,
}

impl PreviewEngine {
    fn new(
        py_exec: PathBuf,
        worker_py: PathBuf,
        version: watch::Sender<Option<i64>>,
        unitypy: watch::Sender<Option<bool>>,
    ) -> Self {
        PreviewEngine {
            py_exec,
            worker_py,
            version,
            unitypy,
            info_timeout: ENGINE_INFO_TIMEOUT,
            worker: None,
            starts: 0,
            failures: 0,
            retry_at: None,
        }
    }

    /// Whether a worker is serving, after starting one when none is and a start is due.
    async fn ready(&mut self) -> bool {
        while self.worker.is_none() {
            if self.retry_at.is_some_and(|at| Instant::now() < at) {
                return false;
            }
            self.start().await;
        }
        true
    }

    /// Starts a worker process and publishes the version it answers to `engine_info`. One that
    /// can't start, exits or times out is killed, as a job timeout does, and publishes nothing,
    /// and the next start waits `restart_delay`.
    async fn start(&mut self) {
        self.unpublish();
        self.starts += 1;
        let worker = match PythonWorker::spawn(&self.py_exec, &self.worker_py).await {
            Ok(mut worker) => {
                let req = json!({"id": "engine_info", "method": "engine_info", "params": {}});
                match tokio::time::timeout(self.info_timeout, worker.execute(&req)).await {
                    Ok(Ok(reply)) => {
                        self.unitypy.send_replace(reported_unitypy(&reply));
                        Some((worker, reported_version(&reply)))
                    }
                    Ok(Err(e)) => {
                        tracing::error!("Python worker failed to answer engine_info: {}", e);
                        worker.kill().await;
                        None
                    }
                    Err(_) => {
                        tracing::error!(
                            "Python worker didn't answer engine_info within {:?}. Killing worker.",
                            self.info_timeout
                        );
                        worker.kill().await;
                        None
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to spawn Python preview worker: {}", e);
                None
            }
        };

        match worker {
            Some((worker, version)) => {
                tracing::info!("Preview engine version {} is serving the queue", version);
                self.worker = Some(worker);
                self.failures = 0;
                self.retry_at = None;
                self.version.send_replace(Some(version));
            }
            None => {
                self.failures += 1;
                let delay = restart_delay(self.failures);
                if !delay.is_zero() {
                    tracing::warn!("Starting the Python preview worker again in {:?}", delay);
                }
                self.retry_at = Some(Instant::now() + delay);
            }
        }
    }

    /// Runs one job's request on the serving worker, which `ready` has started. A worker that
    /// fails or takes over `JOB_TIMEOUT` is killed, and the next `ready` starts a new one.
    async fn run(&mut self, req: &serde_json::Value, source: &Path) -> Result<serde_json::Value> {
        let Some(worker) = self.worker.as_mut() else {
            return Err(anyhow!("Python worker unavailable"));
        };
        let err = match tokio::time::timeout(JOB_TIMEOUT, worker.execute(req)).await {
            Ok(Ok(val)) => return Ok(val),
            Ok(Err(e)) => {
                tracing::error!("Python worker execute error: {}. Restarting worker.", e);
                e
            }
            Err(_) => {
                tracing::error!(
                    "Python worker timed out after {:?} processing {}. Killing worker.",
                    JOB_TIMEOUT,
                    source.display()
                );
                anyhow!("Job timed out")
            }
        };
        if let Some(mut worker) = self.worker.take() {
            worker.kill().await;
        }
        self.unpublish();
        Err(err)
    }

    /// Publishes "none yet", without notifying receivers when it already holds that.
    fn unpublish(&self) {
        self.version.send_if_modified(|v| v.take().is_some());
    }
}

/// A request for the Python queue's worker from elsewhere in the app, such as the game
/// export's `fbx_facts`, and where its reply goes: the worker's reply line, or why there is
/// none. The queue serves these between jobs, on the one worker process.
#[derive(Debug)]
pub struct WorkerRequest {
    pub request: serde_json::Value,
    pub reply: oneshot::Sender<std::result::Result<serde_json::Value, String>>,
}

/// Runs `request` on `engine`'s worker, starting one when none is serving and a start is due.
async fn serve(engine: &mut PreviewEngine, WorkerRequest { request, reply }: WorkerRequest) {
    let answer = if engine.ready().await {
        let source = request["params"]["source"].as_str().unwrap_or_default();
        let source = PathBuf::from(source);
        engine
            .run(&request, &source)
            .await
            .map_err(|e| e.to_string())
    } else {
        Err("the preview engine isn't running".to_string())
    };
    let _ = reply.send(answer);
}

/// Serves every request waiting in `requests`.
async fn serve_waiting(engine: &mut PreviewEngine, requests: &mut mpsc::Receiver<WorkerRequest>) {
    while let Ok(request) = requests.try_recv() {
        serve(engine, request).await;
    }
}

/// Runs 3D/CAD/DCC formats (and images Rust couldn't decode) through the persistent Python
/// worker, and serves `requests` on it between jobs, or at once while it is idle.
async fn run_python_queue(
    state: Arc<AppState>,
    py_exec: PathBuf,
    worker_py: PathBuf,
    mut requests: mpsc::Receiver<WorkerRequest>,
) {
    let mut engine = PreviewEngine::new(
        py_exec,
        worker_py,
        state.engine_version.clone(),
        state.engine_unitypy.clone(),
    );
    loop {
        let libs = state.db.get_all_libraries().await;
        let processed_any =
            python_queue_round(&libs, &mut engine, &mut requests, |outcome| match outcome {
                Ok(done) => emit_done(&state, &done),
                Err(failed) => emit_error(&state, &failed),
            })
            .await;

        if !processed_any {
            tokio::select! {
                Some(request) = requests.recv() => serve(&mut engine, request).await,
                _ = tokio::time::sleep(Duration::from_millis(1000)) => {}
            }
        }
    }
}

/// One round of the Python queue. It starts `engine`'s worker when none is serving and a start
/// is due, with or without jobs waiting, then runs up to 10 of each library's jobs, passing each
/// one's outcome to `finished`, and serves the waiting `requests` first and after each job.
/// Jobs wait while no worker serves: each is claimed only once a worker is ready for it, so a
/// worker that dies fails only the job it was running, and a round whose restart backs off
/// leaves the rest queued. Returns whether any job was claimed.
async fn python_queue_round(
    libs: &[LibraryHandle],
    engine: &mut PreviewEngine,
    requests: &mut mpsc::Receiver<WorkerRequest>,
    mut finished: impl FnMut(Result<PreviewDone, FailedJob>),
) -> bool {
    serve_waiting(engine, requests).await;
    if !engine.ready().await {
        return false;
    }
    let filter = format!(
        "(p.status IN ('queued', 'pending') AND lower(coalesce(a.extension, '')) NOT IN {}) OR p.status = 'fallback'",
        NATIVE_EXTS_SQL
    );
    let mut processed_any = false;

    for lib in libs {
        let pool = &lib.pool;
        let root_path = &lib.root_path;
        let thumb_dir = root_path.join(".opendam").join("thumbs");
        let _ = tokio::fs::create_dir_all(&thumb_dir).await;

        for _ in 0..10 {
            serve_waiting(engine, requests).await;
            if !engine.ready().await {
                return processed_any;
            }
            let Some((job_id, asset_id, file_path)) = claim_jobs(pool, &filter, 1).await.pop()
            else {
                break;
            };
            processed_any = true;
            let full_source = resolve_source(root_path, &file_path);
            let req = model_meta_request(lib, &job_id, &asset_id, &file_path).await;
            let py_resp = engine.run(&req, &full_source).await;

            let mut success = false;
            let mut result_thumb = None;
            let mut result_model = None;
            let mut report = EngineReport::default();

            if let Ok(resp) = py_resp {
                if let Some(res) = resp.get("result") {
                    if res.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                        success = true;
                        if let Some(thumb) = res.get("thumbnail").and_then(|v| v.as_str()) {
                            result_thumb = Some(thumb.to_string());
                        }
                        if let Some(model) = res.get("preview_model").and_then(|v| v.as_str()) {
                            result_model = Some(model.to_string());
                        }
                        report = EngineReport::of_result(res);
                    } else {
                        tracing::error!("Python returned ok=false: {:?}", res);
                    }
                } else {
                    tracing::error!("Python response missing result: {:?}", resp);
                }
            }

            if success {
                tracing::info!(
                    "Job {} succeeded, thumb: {:?}, model: {:?}",
                    job_id,
                    result_thumb,
                    result_model
                );
                let rel_thumb = result_thumb
                    .or_else(|| result_model.clone())
                    .map(|abs| make_relative_path(root_path, Path::new(&abs)));
                let done = complete_job(
                    pool,
                    root_path,
                    &job_id,
                    &asset_id,
                    rel_thumb,
                    result_model,
                    &report,
                )
                .await;
                finished(Ok(done));
            } else {
                fail_job(pool, &job_id, &asset_id).await;
                finished(Err((job_id, asset_id)));
            }

            // Yield execution between items to allow other async runtime tasks to progress
            tokio::task::yield_now().await;
        }
    }
    processed_any
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_panic_becomes_err() {
        let res: Result<()> = catch_decoder_panic(|| panic!("malformed image"));
        assert_eq!(res.unwrap_err().to_string(), "image decoder panicked");
    }

    /// A preview engine at `dir`: `worker.py`, plus `.venv/bin/python` when `venv`.
    fn engine(dir: &Path, venv: bool) -> PathBuf {
        std::fs::create_dir_all(dir.join(".venv").join("bin")).unwrap();
        std::fs::write(dir.join("worker.py"), "").unwrap();
        if venv {
            std::fs::write(dir.join(".venv").join("bin").join("python"), "").unwrap();
        }
        dir.to_path_buf()
    }

    fn lookup(vars: &[(&str, &Path)], roots: &[PathBuf]) -> (PathBuf, PathBuf) {
        let vars: std::collections::HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_os_str().to_owned()))
            .collect();
        find_preview_worker(|k| vars.get(k).cloned(), roots)
    }

    #[test]
    fn preview_worker_lookup_order() {
        let tmp = std::env::temp_dir().join(format!("opendam-lookup-{}", uuid::Uuid::new_v4()));
        let data_home = tmp.join("data");
        let repo = tmp.join("repo");
        let installed = engine(&data_home.join("opendam").join("preview-engine"), true);
        let dev = engine(&repo.join("packages").join("preview-engine"), true);
        let venv_python = |e: &Path| e.join(".venv").join("bin").join("python");
        let roots = [repo.join("apps"), repo.clone()];

        // 1. The env overrides win over an installed engine.
        let (py, worker) = lookup(
            &[
                ("OPENDAM_PYTHON", Path::new("/env/python")),
                ("OPENDAM_WORKER", Path::new("/env/worker.py")),
                ("XDG_DATA_HOME", &data_home),
            ],
            &roots,
        );
        assert_eq!(py, PathBuf::from("/env/python"));
        assert_eq!(worker, PathBuf::from("/env/worker.py"));

        // 2. The installed engine under XDG_DATA_HOME wins over the repo.
        let (py, worker) = lookup(&[("XDG_DATA_HOME", &data_home)], &roots);
        assert_eq!(py, venv_python(&installed));
        assert_eq!(worker, installed.join("worker.py"));

        // Without XDG_DATA_HOME, the installed engine is under ~/.local/share.
        let home = tmp.join("home");
        let home_engine = engine(
            &home
                .join(".local")
                .join("share")
                .join("opendam")
                .join("preview-engine"),
            true,
        );
        let (py, _) = lookup(&[("HOME", &home)], &roots);
        assert_eq!(py, venv_python(&home_engine));

        // 3. With nothing installed, the dev repo's engine.
        let empty = tmp.join("empty");
        let (py, worker) = lookup(&[("XDG_DATA_HOME", &empty)], &roots);
        assert_eq!(py, venv_python(&dev));
        assert_eq!(worker, dev.join("worker.py"));

        // An engine without a .venv runs on python3 from PATH.
        let bin = tmp.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("python3"), "").unwrap();
        let bare = engine(
            &tmp.join("bare").join("packages").join("preview-engine"),
            false,
        );
        let (py, worker) = lookup(
            &[("XDG_DATA_HOME", &empty), ("PATH", &bin)],
            &[tmp.join("bare")],
        );
        assert_eq!(py, bin.join("python3"));
        assert_eq!(worker, bare.join("worker.py"));

        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[tokio::test]
    async fn forward_lines_survives_invalid_utf8() {
        let mut lines = Vec::new();
        forward_lines(&b"first\nbad \xff byte\r\nlast"[..], |l| {
            lines.push(l.to_string())
        })
        .await;
        assert_eq!(lines, ["first", "bad \u{fffd} byte", "last"]);
    }

    /// Writes a `width` x `height` image to `path`, in the format its extension names.
    fn write_image(path: &Path, width: u32, height: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::RgbImage::new(width, height).save(path).unwrap();
    }

    #[test]
    fn small_displayable_image_gets_no_thumbnail() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("icon.png");
        write_image(&source, 64, 32);
        let thumbs = dir.path().join("thumbs");

        let name = generate_image_thumb(&source, &thumbs, "a1").unwrap();

        assert_eq!(name, None);
        assert!(!thumbs.exists());
    }

    #[test]
    fn small_tga_gets_a_native_size_png() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("sprite.tga");
        write_image(&source, 48, 20);
        let thumbs = dir.path().join("thumbs");

        let name = generate_image_thumb(&source, &thumbs, "a1").unwrap();

        assert_eq!(name.as_deref(), Some("a1_thumb.png"));
        assert_eq!(
            image::image_dimensions(thumbs.join("a1_thumb.png")).unwrap(),
            (48, 20)
        );
    }

    #[test]
    fn large_png_is_downscaled_to_512() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("rock.png");
        write_image(&source, 1024, 40);
        let thumbs = dir.path().join("thumbs");

        let name = generate_image_thumb(&source, &thumbs, "a1").unwrap();

        assert_eq!(name.as_deref(), Some("a1_thumb.png"));
        assert_eq!(
            image::image_dimensions(thumbs.join("a1_thumb.png")).unwrap(),
            (512, 20)
        );
    }

    #[test]
    fn image_of_exactly_512_is_not_resized() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("edge.png");
        let tif = dir.path().join("edge.tif");
        write_image(&png, 512, 8);
        write_image(&tif, 512, 100);
        let thumbs = dir.path().join("thumbs");

        assert_eq!(generate_image_thumb(&png, &thumbs, "p").unwrap(), None);
        assert_eq!(
            generate_image_thumb(&tif, &thumbs, "t").unwrap().as_deref(),
            Some("t_thumb.png")
        );
        assert_eq!(
            image::image_dimensions(thumbs.join("t_thumb.png")).unwrap(),
            (512, 100)
        );
    }

    /// A library at `root` holding asset `a1` (the source `file_path`) with `old_thumb` as its
    /// thumbnail and a pending preview job `j1`.
    async fn library_with_pending_job(
        root: &Path,
        file_path: &str,
        old_thumb: &str,
    ) -> LibraryHandle {
        let pool = crate::db::memory_library_pool().await;
        let ext = file_path.rsplit('.').next().unwrap();
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, thumbnail_path, preview_status)
             VALUES ('a1', ?, ?, ?, 'texture', 0, 0, 0, ?, 'pending')",
        )
        .bind(file_path)
        .bind(file_path.rsplit('/').next().unwrap())
        .bind(ext)
        .bind(old_thumb)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO preview_jobs (id, asset_id, status, created_at) VALUES ('j1', 'a1', 'pending', 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        LibraryHandle {
            id: "lib".into(),
            name: "lib".into(),
            root_path: root.to_path_buf(),
            pool,
        }
    }

    /// Asset `a1`'s (thumbnail_path, preview_status) and job `j1`'s status.
    async fn job_outcome(pool: &Pool<Sqlite>) -> (Option<String>, String, String) {
        let (thumb, status): (Option<String>, String) =
            sqlx::query_as("SELECT thumbnail_path, preview_status FROM assets WHERE id = 'a1'")
                .fetch_one(pool)
                .await
                .unwrap();
        let job: String = sqlx::query_scalar("SELECT status FROM preview_jobs WHERE id = 'j1'")
            .fetch_one(pool)
            .await
            .unwrap();
        (thumb, status, job)
    }

    /// A library at `/lib` holding `folders` rows, each `(path, is_pack)`.
    async fn library_with_folders(folders: &[(&str, bool)]) -> LibraryHandle {
        let pool = crate::db::memory_library_pool().await;
        for (path, is_pack) in folders {
            sqlx::query("INSERT INTO folders (path, name, is_pack) VALUES (?, ?, ?)")
                .bind(path)
                .bind(path.rsplit('/').next().unwrap())
                .bind(is_pack)
                .execute(&pool)
                .await
                .unwrap();
        }
        LibraryHandle {
            id: "lib".into(),
            name: "lib".into(),
            root_path: PathBuf::from("/lib"),
            pool,
        }
    }

    #[tokio::test]
    async fn pack_root_is_the_innermost_pack_above_the_asset() {
        let lib = library_with_folders(&[
            ("", false),
            ("Assets", true),
            ("Assets/100%_Pack", true),
            ("Assets/100%_Pack/Meshes", false),
            // `LIKE 'Assets/100%_Pack/%'` would take this folder's assets as the pack's.
            ("Assets/100xyPack", false),
            ("Assets/100xyPack/Meshes", false),
            ("AssetsX", false),
            ("Loose", false),
        ])
        .await;
        let pack = |file: &'static str| pack_root(&lib.pool, file);

        assert_eq!(
            pack("Assets/100%_Pack/Meshes/rock.fbx").await.as_deref(),
            Some("Assets/100%_Pack")
        );
        assert_eq!(
            pack("Assets/100%_Pack/rock.fbx").await.as_deref(),
            Some("Assets/100%_Pack")
        );
        assert_eq!(
            pack("Assets/100xyPack/Meshes/rock.fbx").await.as_deref(),
            Some("Assets")
        );
        assert_eq!(pack("AssetsX/rock.fbx").await, None);
        assert_eq!(pack("Loose/rock.fbx").await, None);
        assert_eq!(pack("rock.fbx").await, None);
    }

    #[tokio::test]
    async fn pack_root_takes_a_library_root_marked_as_a_pack() {
        let lib = library_with_folders(&[("", true), ("Meshes", false)]).await;
        assert_eq!(
            pack_root(&lib.pool, "Meshes/rock.fbx").await.as_deref(),
            Some("")
        );
        assert_eq!(pack_root(&lib.pool, "rock.fbx").await.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn model_meta_sends_the_library_and_pack_roots() {
        let lib = library_with_folders(&[("Kit", true), ("Loose", false)]).await;

        let req = model_meta_request(&lib, "j1", "a1", "Kit/Models/crate.fbx").await;
        assert_eq!(
            req,
            json!({
                "id": "j1",
                "method": "model_meta",
                "params": {
                    "source": "/lib/Kit/Models/crate.fbx",
                    "output_dir": "/lib/.opendam/thumbs",
                    "name": "a1",
                    "library_root": "/lib",
                    "pack_root": "/lib/Kit",
                }
            })
        );

        let req = model_meta_request(&lib, "j2", "a2", "Loose/crate.fbx").await;
        assert_eq!(req["params"]["library_root"], "/lib");
        assert!(req["params"].get("pack_root").is_none());
    }

    #[tokio::test]
    async fn claiming_skips_a_group_members_job() {
        let pool = crate::db::memory_library_pool().await;
        // As the structure pass leaves them: the main item waits for its preview
        for (id, group) in [("glb", Some("glb")), ("fbx", Some("glb")), ("png", None)] {
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, group_id, preview_status)
                 VALUES (?, ?, ?, ?, 'texture', 1, 0, 0, ?, 'pending')",
            )
            .bind(id)
            .bind(format!("{id}/a.{id}"))
            .bind(format!("a.{id}"))
            .bind(id)
            .bind(group)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO preview_jobs (id, asset_id, status, created_at) VALUES (?, ?, 'pending', 0)")
                .bind(format!("job-{id}"))
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }
        let mut claimed: Vec<String> = claim_jobs(&pool, "p.status = 'pending'", 10)
            .await
            .into_iter()
            .map(|(_, asset, _)| asset)
            .collect();
        claimed.sort();
        assert_eq!(claimed, ["glb", "png"]);
    }

    #[tokio::test]
    async fn claiming_takes_a_members_job_when_its_main_item_has_no_preview() {
        let pool = crate::db::memory_library_pool().await;
        for (id, ext, group) in [
            ("blend", "blend", "blend"),
            ("obj", "obj", "blend"),
            ("glb", "glb", "glb"),
            ("fbx", "fbx", "glb"),
        ] {
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, group_id, preview_status)
                 VALUES (?, ?, ?, ?, '3d_model', 1, 0, 0, ?, ?)",
            )
            .bind(id)
            .bind(format!("{id}/a.{ext}"))
            .bind(format!("a.{ext}"))
            .bind(ext)
            .bind(group)
            .bind(if id == "glb" { "pending" } else { "none" })
            .execute(&pool)
            .await
            .unwrap();
        }
        for id in ["obj", "glb", "fbx"] {
            sqlx::query("INSERT INTO preview_jobs (id, asset_id, status, created_at) VALUES (?, ?, 'pending', 0)")
                .bind(format!("job-{id}"))
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }
        let mut claimed: Vec<String> = claim_jobs(&pool, "p.status = 'pending'", 10)
            .await
            .into_iter()
            .map(|(_, asset, _)| asset)
            .collect();
        claimed.sort();
        assert_eq!(claimed, ["glb", "obj"]);
    }

    /// An item's id, extension, group and its render's size when it records one.
    type RenderedItem<'a> = (&'a str, &'a str, Option<&'a str>, Option<(u32, u32)>);

    /// The pending jobs `claim_jobs` claims from a library holding `items`, each an id, its
    /// extension, its group, and its render's size when it records one, at `Previews/<id>.png`.
    /// Every item but a group's main item has a job.
    async fn claimed_with_renders(items: &[RenderedItem<'_>]) -> Vec<String> {
        let pool = crate::db::memory_library_pool().await;
        for &(id, ext, group, render) in items {
            let render_path = format!("Previews/{id}.png");
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, group_id, bundled_preview, preview_status)
                 VALUES (?, ?, ?, ?, '3d_model', 1, 0, 0, ?, ?, 'none')",
            )
            .bind(id)
            .bind(format!("{id}/a.{ext}"))
            .bind(format!("a.{ext}"))
            .bind(ext)
            .bind(group)
            .bind(render.map(|_| &render_path))
            .execute(&pool)
            .await
            .unwrap();
            if let Some((width, height)) = render {
                sqlx::query(
                    "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, support_role, preview_status, render_width, render_height, render_read_bytes, render_read_mtime)
                     VALUES (?, ?, ?, 'png', 'texture', 1, 0, 0, 'model_preview', 'done', ?, ?, 1, 0)",
                )
                .bind(&render_path)
                .bind(&render_path)
                .bind(format!("{id}.png"))
                .bind(width)
                .bind(height)
                .execute(&pool)
                .await
                .unwrap();
            }
            if group.is_none_or(|g| g != id) {
                sqlx::query("INSERT INTO preview_jobs (id, asset_id, status, created_at) VALUES (?, ?, 'pending', 0)")
                    .bind(format!("job-{id}"))
                    .bind(id)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
        }
        let mut claimed: Vec<String> = claim_jobs(&pool, "p.status = 'pending'", 10)
            .await
            .into_iter()
            .map(|(_, asset, _)| asset)
            .collect();
        claimed.sort();
        claimed
    }

    #[tokio::test]
    async fn claiming_skips_a_job_for_an_item_with_a_bundled_preview() {
        // `rock` shows its render, which fits, and `blend` leads a group whose OBJ would stand
        // in, but the group has a render that fits too. Only `tree` has no picture.
        let claimed = claimed_with_renders(&[
            ("rock", "glb", None, Some((512, 512))),
            ("blend", "blend", Some("blend"), Some((1024, 768))),
            ("obj", "obj", Some("blend"), None),
            ("tree", "glb", None, None),
        ])
        .await;
        assert_eq!(claimed, ["tree"]);
    }

    #[tokio::test]
    async fn claiming_takes_a_job_for_an_item_whose_render_is_too_small() {
        // `car`'s render is Kenney's 64 px one and `van`'s is under 512 px tall, so both are
        // rendered. So is `obj`, the stand-in of a `.blend` group whose render is too small.
        // `bus`'s render fits, so its job is skipped.
        let claimed = claimed_with_renders(&[
            ("car", "glb", None, Some((64, 64))),
            ("van", "glb", None, Some((1024, 300))),
            ("bus", "glb", None, Some((600, 512))),
            ("blend", "blend", Some("blend"), Some((64, 64))),
            ("obj", "obj", Some("blend"), None),
        ])
        .await;
        assert_eq!(claimed, ["car", "obj", "van"]);
    }

    #[tokio::test]
    async fn job_for_a_source_that_shrank_deletes_its_old_thumbnail_and_clears_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // The old thumbnail, from when the source was larger.
        let old_thumb = root.join(".opendam/thumbs/a1_thumb.png");
        write_image(&old_thumb, 16, 16);
        write_image(&root.join("textures/rock.png"), 256, 256);
        let lib =
            library_with_pending_job(root, "textures/rock.png", ".opendam/thumbs/a1_thumb.png")
                .await;

        let (claimed, done) = process_image_jobs(&lib, 2).await;

        assert!(claimed);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].asset_id, "a1");
        assert_eq!(done[0].thumb_path, None);
        // The thumbnail it had, so a model card showing it as a render can drop it.
        assert_eq!(
            done[0].previous_thumb_path.as_deref(),
            Some(old_thumb.to_string_lossy().as_ref())
        );
        assert!(!old_thumb.exists());
        assert_eq!(
            job_outcome(&lib.pool).await,
            (None, "done".into(), "done".into())
        );
    }

    #[tokio::test]
    async fn job_for_a_large_source_keeps_its_rewritten_same_named_thumbnail() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // The old thumbnail has the name the new one is written under.
        let thumb = root.join(".opendam/thumbs/a1_thumb.png");
        write_image(&thumb, 30, 30);
        write_image(&root.join("textures/rock.png"), 1024, 32);
        let lib =
            library_with_pending_job(root, "textures/rock.png", ".opendam/thumbs/a1_thumb.png")
                .await;

        let (_, done) = process_image_jobs(&lib, 2).await;

        assert_eq!(
            done[0].thumb_path.as_deref(),
            Some(thumb.to_string_lossy().as_ref())
        );
        // The source's own path, so a model card showing it as a render takes its thumbnail.
        assert_eq!(
            done[0].file_path.as_deref(),
            Some(root.join("textures/rock.png").to_string_lossy().as_ref())
        );
        assert_eq!(image::image_dimensions(&thumb).unwrap(), (512, 16));
        assert_eq!(
            job_outcome(&lib.pool).await,
            (
                Some(".opendam/thumbs/a1_thumb.png".into()),
                "done".into(),
                "done".into()
            )
        );
    }

    #[tokio::test]
    async fn job_that_renames_a_thumbnail_deletes_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // An upscaled JPEG from before the rule; a small TGA now gets a native-size PNG.
        let old_thumb = root.join(".opendam/thumbs/a1_thumb.jpg");
        write_image(&old_thumb, 16, 16);
        write_image(&root.join("textures/rock.tga"), 64, 64);
        let lib =
            library_with_pending_job(root, "textures/rock.tga", ".opendam/thumbs/a1_thumb.jpg")
                .await;

        process_image_jobs(&lib, 2).await;

        assert!(!old_thumb.exists());
        let new_thumb = root.join(".opendam/thumbs/a1_thumb.png");
        assert_eq!(image::image_dimensions(&new_thumb).unwrap(), (64, 64));
        assert_eq!(
            job_outcome(&lib.pool).await,
            (
                Some(".opendam/thumbs/a1_thumb.png".into()),
                "done".into(),
                "done".into()
            )
        );
    }

    #[tokio::test]
    async fn model_job_deletes_a_renamed_thumb_but_keeps_its_new_model() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let thumbs = root.join(".opendam/thumbs");
        // The converted model from an earlier Python job, and an old JPEG thumbnail.
        let model = thumbs.join("a1_preview.glb");
        let old_thumb = thumbs.join("a1_thumb.jpg");
        std::fs::create_dir_all(&thumbs).unwrap();
        std::fs::write(&model, "glb").unwrap();
        write_image(&old_thumb, 64, 64);
        let lib =
            library_with_pending_job(root, "models/ship.fbx", ".opendam/thumbs/a1_thumb.jpg").await;

        // A re-preview with no thumbnail stores the model as one, as Python jobs do.
        let model_abs = model.to_string_lossy().to_string();
        complete_job(
            &lib.pool,
            root,
            "j1",
            "a1",
            Some(".opendam/thumbs/a1_preview.glb".into()),
            Some(model_abs.clone()),
            &EngineReport::default(),
        )
        .await;
        assert!(!old_thumb.exists());
        assert!(model.exists());

        // A later job that writes a thumbnail keeps the model the row pointed at, which is
        // still its `preview_model`.
        complete_job(
            &lib.pool,
            root,
            "j1",
            "a1",
            Some(".opendam/thumbs/a1_thumb.png".into()),
            Some(model_abs),
            &EngineReport::default(),
        )
        .await;
        assert!(model.exists());
    }

    #[tokio::test]
    async fn a_job_stores_the_engines_report_and_one_without_it_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let lib = library_with_pending_job(root, "models/ship.fbx", "").await;
        let textures = json!({
            "materials": 3, "textured": 2, "via": {"relative": 1, "name": 1},
            "missing": ["T_Fill_B"], "unreadable": [], "too_large": [], "no_uv": 0,
        });
        let current = json!({"ok": true, "preview_version": 2, "preview_textures": textures});
        let model = root.join(".opendam/thumbs/a1_preview.glb");
        let stored = || async {
            let (version, report): (Option<i64>, Option<String>) =
                sqlx::query_as("SELECT preview_version, preview_textures FROM assets")
                    .fetch_one(&lib.pool)
                    .await
                    .unwrap();
            let report = report.map(|r| serde_json::from_str::<serde_json::Value>(&r).unwrap());
            (version, report)
        };

        // With and without a converted model, as Python and Rust jobs complete
        for model_path in [Some(model.to_string_lossy().to_string()), None] {
            let thumb = Some(".opendam/thumbs/a1_preview.glb".to_string());
            let complete = |result: serde_json::Value| {
                let (thumb, model_path) = (thumb.clone(), model_path.clone());
                let lib = &lib;
                async move {
                    let report = EngineReport::of_result(&result);
                    complete_job(&lib.pool, root, "j1", "a1", thumb, model_path, &report).await;
                }
            };

            complete(current.clone()).await;
            assert_eq!(stored().await, (Some(2), Some(textures.clone())));

            // An older engine's result, which reads as version 1
            complete(json!({"ok": true})).await;
            assert_eq!(stored().await, (None, None));
        }
    }

    // Stub workers: sh snippets that answer one request, its `id` in `$id`. `$n` counts the
    // processes started, and `$DIR` is the stub's folder, the library root in these tests.
    const ANSWER_2: &str = r#"printf '{"id":"%s","result":{"preview_version":2}}\n' "$id""#;
    const UNKNOWN_METHOD: &str =
        r#"printf '{"id":"%s","error":"Unknown method: engine_info"}\n' "$id""#;
    const JOB_DONE: &str = r#"printf '{"id":"%s","result":{"ok":true,"preview_model":"%s/.opendam/thumbs/%s.glb","preview_version":2}}\n' "$id" "$DIR" "$id""#;

    /// A preview engine whose worker runs `engine_info` for that method and `job` for any other.
    fn stub_engine(dir: &Path, engine_info: &str, job: &str) -> PreviewEngine {
        let script = dir.join("worker.sh");
        std::fs::write(
            &script,
            format!(
                r#"DIR=$(dirname "$0")
n=$(( $(cat "$DIR/starts" 2>/dev/null || echo 0) + 1 ))
echo $n > "$DIR/starts"
while read -r line; do
  id=$(printf '%s\n' "$line" | sed 's/^{{"id":"\([^"]*\)".*/\1/')
  case "$line" in
    *'"method":"engine_info"'*) {engine_info} ;;
    *) {job} ;;
  esac
done
"#
            ),
        )
        .unwrap();
        PreviewEngine::new(
            "sh".into(),
            script,
            watch::Sender::new(None),
            watch::Sender::new(None),
        )
    }

    /// The outcomes of one round of the Python queue over `libs`.
    async fn queue_round(
        engine: &mut PreviewEngine,
        libs: &[LibraryHandle],
    ) -> Vec<Result<PreviewDone, FailedJob>> {
        let mut outcomes = Vec::new();
        let (_, mut requests) = mpsc::channel(1);
        python_queue_round(libs, engine, &mut requests, |o| outcomes.push(o)).await;
        outcomes
    }

    /// Runs `lib`'s pending job `j1` and checks it got its own result from a version 2 engine.
    async fn assert_job_gets_its_own_result(engine: &mut PreviewEngine, lib: &LibraryHandle) {
        let outcomes = queue_round(engine, std::slice::from_ref(lib)).await;
        let [Ok(done)] = &outcomes[..] else {
            panic!("one job done: {outcomes:?}");
        };
        let model = lib.root_path.join(".opendam/thumbs/j1.glb");
        assert_eq!(done.model_path, Some(model.to_string_lossy().into_owned()));
        let (status, version): (String, Option<i64>) =
            sqlx::query_as("SELECT preview_status, preview_version FROM assets WHERE id = 'a1'")
                .fetch_one(&lib.pool)
                .await
                .unwrap();
        assert_eq!((status.as_str(), version), ("done", Some(2)));
    }

    #[tokio::test]
    async fn the_queue_publishes_its_workers_engine_version() {
        let with_unitypy =
            r#"printf '{"id":"%s","result":{"preview_version":2,"unitypy":false}}\n' "$id""#;
        for (engine_info, published, unitypy) in [
            (ANSWER_2, 2, None),
            (with_unitypy, 2, Some(false)),
            (UNKNOWN_METHOD, 1, None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut engine = stub_engine(dir.path(), engine_info, JOB_DONE);
            assert_eq!(*engine.version.borrow(), None, "none yet");

            // The worker starts and is asked with no job waiting
            assert!(queue_round(&mut engine, &[]).await.is_empty());
            assert_eq!(*engine.version.borrow(), Some(published));
            assert_eq!(*engine.unitypy.borrow(), unitypy, "{engine_info}");

            // An older worker answering `Unknown method` keeps serving jobs
            let lib = library_with_pending_job(dir.path(), "models/ship.fbx", "").await;
            assert_job_gets_its_own_result(&mut engine, &lib).await;
            assert_eq!(engine.starts, 1);
        }
    }

    #[tokio::test]
    async fn a_worker_answering_engine_info_late_is_killed_and_replaced_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let late = format!(r#"if [ "$n" = 1 ]; then sleep 5; fi; {ANSWER_2}"#);
        let mut engine = stub_engine(dir.path(), &late, JOB_DONE);
        engine.info_timeout = Duration::from_secs(1);
        let published = engine.version.subscribe();

        engine.start().await;
        assert!(engine.worker.is_none(), "the late worker is killed");
        assert_eq!(*engine.version.borrow(), None, "none yet stays");
        assert!(!published.has_changed().unwrap(), "nothing is published");

        // With no job waiting, a new process starts at once and its answer is published
        let started = Instant::now();
        assert!(queue_round(&mut engine, &[]).await.is_empty());
        assert!(started.elapsed() < engine.info_timeout);
        assert_eq!((engine.starts, *engine.version.borrow()), (2, Some(2)));

        let lib = library_with_pending_job(dir.path(), "models/ship.fbx", "").await;
        assert_job_gets_its_own_result(&mut engine, &lib).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_worker_that_never_answers_is_restarted_on_the_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = stub_engine(dir.path(), "exec sleep 3600", JOB_DONE);
        let published = engine.version.subscribe();

        // The first process times out, and the one started at once after it too
        assert!(queue_round(&mut engine, &[]).await.is_empty());
        assert_eq!(engine.starts, 2);

        let mut starts = 2;
        for delay in [10, 30, 60, 300, 300] {
            tokio::time::sleep(Duration::from_secs(delay - 1)).await;
            queue_round(&mut engine, &[]).await;
            assert_eq!(engine.starts, starts, "no start before {delay} s");

            tokio::time::sleep(Duration::from_secs(1)).await;
            queue_round(&mut engine, &[]).await;
            starts += 1;
            assert_eq!(engine.starts, starts, "a start after {delay} s");
        }
        assert!(!published.has_changed().unwrap(), "nothing is published");
        assert_eq!(*engine.version.borrow(), None);
    }

    #[tokio::test]
    async fn a_job_that_kills_the_worker_unpublishes_and_a_new_one_starts_at_once() {
        let dir = tempfile::tempdir().unwrap();
        // Every process exits on its job, as a crash does
        let mut engine = stub_engine(dir.path(), ANSWER_2, "exit 1");
        let lib = library_with_pending_job(dir.path(), "models/ship.fbx", "").await;

        let outcomes = queue_round(&mut engine, std::slice::from_ref(&lib)).await;
        assert!(
            matches!(&outcomes[..], [Err((job, asset))] if job == "j1" && asset == "a1"),
            "the job fails: {outcomes:?}"
        );
        let (_, status, job) = job_outcome(&lib.pool).await;
        assert_eq!((status.as_str(), job.as_str()), ("error", "error"));
        // The same round starts a new process, without waiting for a job, and publishes again
        assert_eq!((engine.starts, *engine.version.borrow()), (2, Some(2)));

        // A failed job kills its worker, and "none yet" holds until a new process answers
        let req = json!({"id": "j2", "method": "model_meta", "params": {}});
        assert!(engine.run(&req, Path::new("ship.fbx")).await.is_err());
        assert!(engine.worker.is_none(), "the worker is killed");
        assert_eq!(*engine.version.borrow(), None);

        assert!(queue_round(&mut engine, &[]).await.is_empty());
        assert_eq!((engine.starts, *engine.version.borrow()), (3, Some(2)));
    }

    #[tokio::test]
    async fn jobs_wait_unclaimed_while_no_worker_serves() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = stub_engine(dir.path(), "exec sleep 3600", JOB_DONE);
        engine.info_timeout = Duration::from_millis(200);
        let lib = library_with_pending_job(dir.path(), "models/ship.fbx", "").await;

        // Both starts time out, so the engine backs off with the job still waiting
        let outcomes = queue_round(&mut engine, std::slice::from_ref(&lib)).await;
        assert!(outcomes.is_empty(), "no job is claimed: {outcomes:?}");
        assert_eq!(engine.starts, 2);
        assert!(engine.worker.is_none());
        let (_, status, job) = job_outcome(&lib.pool).await;
        assert_eq!((status.as_str(), job.as_str()), ("pending", "pending"));
    }

    #[tokio::test]
    async fn jobs_wait_when_the_worker_dies_mid_round_and_its_restarts_back_off() {
        let dir = tempfile::tempdir().unwrap();
        // The first process serves until its job kills it; later ones never answer
        let engine_info = format!(r#"if [ "$n" = 1 ]; then {ANSWER_2}; else exec sleep 3600; fi"#);
        let mut engine = stub_engine(dir.path(), &engine_info, "exit 1");
        engine.info_timeout = Duration::from_millis(200);
        let a = library_with_pending_job(dir.path(), "models/a.fbx", "").await;
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, preview_status)
             VALUES ('a2', 'models/a2.fbx', 'a2.fbx', 'fbx', 'model', 0, 0, 0, 'pending');
             INSERT INTO preview_jobs (id, asset_id, status, created_at) VALUES ('j2', 'a2', 'pending', 0)",
        )
        .execute(&a.pool)
        .await
        .unwrap();
        let b = library_with_pending_job(dir.path(), "models/b.fbx", "").await;

        let outcomes = queue_round(&mut engine, &[a.clone(), b.clone()]).await;
        let [Err(failed)] = &outcomes[..] else {
            panic!("only the running job fails: {outcomes:?}");
        };
        assert_eq!(
            engine.starts, 3,
            "the worker and two restarts that time out"
        );
        assert!(engine.retry_at.is_some_and(|at| at > Instant::now()));

        let statuses = |pool: Pool<Sqlite>| async move {
            sqlx::query_as::<_, (String, String, String)>(
                "SELECT p.id, p.status, a.preview_status FROM preview_jobs p
                 JOIN assets a ON a.id = p.asset_id ORDER BY p.id",
            )
            .fetch_all(&pool)
            .await
            .unwrap()
        };
        let waiting = if failed.0 == "j1" { "j2" } else { "j1" };
        let mut expected = vec![
            (failed.0.clone(), "error".to_string(), "error".to_string()),
            (waiting.to_string(), "pending".into(), "pending".into()),
        ];
        expected.sort();
        assert_eq!(statuses(a.pool.clone()).await, expected);
        assert_eq!(
            statuses(b.pool.clone()).await,
            [(
                "j1".to_string(),
                "pending".to_string(),
                "pending".to_string()
            )],
            "a later library's job isn't claimed"
        );
    }

    #[tokio::test]
    async fn execute_skips_a_reply_to_another_request() {
        let dir = tempfile::tempdir().unwrap();
        let stray = r#"echo 'not json'; printf '{"id":"other","result":{}}\n'; printf '{"id":"%s","result":{"real":true}}\n' "$id""#;
        let mut engine = stub_engine(dir.path(), ANSWER_2, stray);
        assert!(engine.ready().await);

        let worker = engine.worker.as_mut().unwrap();
        let req = json!({"id": "j1", "method": "model_meta", "params": {}});
        let reply = worker.execute(&req).await.unwrap();
        assert_eq!(reply, json!({"id": "j1", "result": {"real": true}}));
    }

    /// Sends `request` to the queue, returning where its reply arrives.
    fn send(
        requests: &mpsc::Sender<WorkerRequest>,
        id: &str,
    ) -> oneshot::Receiver<std::result::Result<serde_json::Value, String>> {
        let (reply, answer) = oneshot::channel();
        let request = json!({"id": id, "method": "fbx_facts", "params": {"source": "a.fbx"}});
        requests.try_send(WorkerRequest { request, reply }).unwrap();
        answer
    }

    #[tokio::test]
    async fn requests_are_served_by_the_queues_worker_beside_its_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = stub_engine(dir.path(), ANSWER_2, JOB_DONE);
        let lib = library_with_pending_job(dir.path(), "models/ship.fbx", "").await;
        let (sender, mut requests) = mpsc::channel(4);
        let first = send(&sender, "facts-1");

        let mut outcomes = Vec::new();
        python_queue_round(
            std::slice::from_ref(&lib),
            &mut engine,
            &mut requests,
            |o| outcomes.push(o),
        )
        .await;

        let reply = first.await.unwrap().unwrap();
        assert_eq!(reply["id"], "facts-1");
        assert!(matches!(&outcomes[..], [Ok(_)]), "{outcomes:?}");
        assert_eq!(engine.starts, 1, "one process serves both");

        // An idle queue serves a request without waiting for a job
        let second = send(&sender, "facts-2");
        let request = requests.recv().await.unwrap();
        serve(&mut engine, request).await;
        assert_eq!(second.await.unwrap().unwrap()["id"], "facts-2");
    }

    #[tokio::test]
    async fn waiting_requests_are_all_answered_before_the_next_job() {
        let dir = tempfile::tempdir().unwrap();
        // The worker logs each request and job id it runs, in order
        let logged = format!(r#"echo "$id" >> "$DIR/order"; {JOB_DONE}"#);
        let mut engine = stub_engine(dir.path(), ANSWER_2, &logged);
        let lib = library_with_pending_job(dir.path(), "models/a.fbx", "").await;
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, preview_status)
             VALUES ('a2', 'models/a2.fbx', 'a2.fbx', 'fbx', 'model', 0, 0, 0, 'pending');
             INSERT INTO preview_jobs (id, asset_id, status, created_at) VALUES ('j2', 'a2', 'pending', 0)",
        )
        .execute(&lib.pool)
        .await
        .unwrap();
        let (sender, mut requests) = mpsc::channel(8);
        let answers: Vec<_> = (1..=3)
            .map(|n| send(&sender, &format!("facts-{n}")))
            .collect();

        let mut outcomes = Vec::new();
        python_queue_round(
            std::slice::from_ref(&lib),
            &mut engine,
            &mut requests,
            |o| outcomes.push(o),
        )
        .await;

        for (n, answer) in answers.into_iter().enumerate() {
            assert_eq!(
                answer.await.unwrap().unwrap()["id"],
                format!("facts-{}", n + 1)
            );
        }
        assert_eq!(outcomes.len(), 2, "{outcomes:?}");
        let order = std::fs::read_to_string(dir.path().join("order")).unwrap();
        let order: Vec<&str> = order.lines().collect();
        assert_eq!(&order[..3], ["facts-1", "facts-2", "facts-3"], "{order:?}");
        assert_eq!(order.len(), 5, "{order:?}");
    }

    #[tokio::test]
    async fn a_request_no_worker_can_serve_is_answered_with_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = stub_engine(dir.path(), "exec sleep 3600", JOB_DONE);
        engine.info_timeout = Duration::from_millis(200);
        let (sender, mut requests) = mpsc::channel(4);
        let answer = send(&sender, "facts-1");

        python_queue_round(&[], &mut engine, &mut requests, |_| {}).await;
        assert_eq!(
            answer.await.unwrap(),
            Err("the preview engine isn't running".to_string())
        );
    }

    #[test]
    fn decoder_result_passes_through() {
        assert!(catch_decoder_panic(|| Ok(())).is_ok());
        let res: Result<()> = catch_decoder_panic(|| Err(anyhow!("bad header")));
        assert_eq!(res.unwrap_err().to_string(), "bad header");
    }
}
