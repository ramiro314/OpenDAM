use crate::commands::asset::{pictured, MAIN_OR_SINGLE};
use crate::db::{make_relative_path, resolve_path, LibraryHandle};
use crate::AppState;
use anyhow::{anyhow, Result};
use image::imageops::FilterType;
use image::ImageError;
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
        "jpg" | "jpeg" | "png" | "bmp" | "gif" | "tif" | "tiff" | "tga" | "webp" | "hdr"
    )
}

/// Whether a file with extension `ext` gets a preview job: models, layered and EXR images, and
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

/// What `generate_image_thumb` made of a raster.
#[derive(Debug, PartialEq)]
pub enum ImageThumb {
    /// A thumbnail, by its file name in the thumbnail folder.
    Written(String),
    /// No thumbnail: the UI shows the original.
    Original,
    /// No preview at all, for this reason, because the raster is past the decoder's memory limit.
    TooLarge(String),
}

/// Writes `asset_id`'s thumbnail for `source` into `thumb_dir`, if it gets one. Thumbnails are
/// never upscaled: a source `THUMB_SIZE` px or smaller gets no thumbnail if the webview can
/// display it, and otherwise (TGA, TIFF, HDR) a PNG at its own size. Larger sources are
/// downsampled with Triangle (bilinear) filtering, 4x-8x faster than Lanczos3.
pub fn generate_image_thumb(source: &Path, thumb_dir: &Path, asset_id: &str) -> Result<ImageThumb> {
    let ext = source
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    // The header alone gives the size, so a small displayable image is never decoded.
    let (width, height) = image::image_dimensions(source)?;
    let small = width.max(height) <= THUMB_SIZE;
    if small && is_webview_displayable(&ext) {
        return Ok(ImageThumb::Original);
    }

    // `open` reserves the decoded size against its limit before it reads a pixel.
    let img = match image::open(source) {
        Err(ImageError::Limits(_)) => {
            return Ok(ImageThumb::TooLarge(format!(
                "Too large to preview ({width} x {height} px)"
            )))
        }
        img => img?,
    };
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

    // After the resize, so it filters linear light and tone maps only the thumbnail's pixels.
    let thumb = if ext == "hdr" {
        tone_map(thumb.into_rgb32f()).into()
    } else {
        thumb
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
    Ok(ImageThumb::Written(file_name))
}

/// Linear light as 8-bit RGB, as the worker's `.exr` branch does it: Reinhard, then gamma 2.2.
fn tone_map(linear: image::Rgb32FImage) -> image::RgbImage {
    let (width, height) = linear.dimensions();
    let bytes = linear
        .into_raw()
        .into_iter()
        .map(|c| ((c / (c + 1.0)).powf(1.0 / 2.2) * 255.0) as u8)
        .collect();
    image::RgbImage::from_raw(width, height, bytes).expect("one byte for each channel")
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
    "('jpg','jpeg','png','bmp','gif','tif','tiff','tga','webp','hdr')";

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

/// The assets whose preview GLB waits for the frontend to render it (`modelThumbnailer`): a first
/// preview holds the GLB as its thumbnail, and a fresh one keeps its old picture, `generating`
/// (`complete_job`).
pub(crate) const AWAITING_RENDER: &str =
    "(thumbnail_path LIKE '%.glb' OR preview_status = 'generating')";

/// An asset awaiting a render (`AWAITING_RENDER`). `model` is its preview GLB, relative to the
/// library root. `revision` counts the GLBs jobs wrote for it, so it is the token its render is
/// saved or failed with: the row no longer holds it once a newer GLB exists.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct AwaitingRender {
    pub id: String,
    pub model: String,
    pub generating: bool,
    pub revision: i64,
}

/// Up to `limit` assets awaiting a render. A `generating` one keeps its old picture as its
/// thumbnail, so its GLB is its `preview_model`.
pub(crate) async fn models_awaiting_render(
    pool: &Pool<Sqlite>,
    limit: i64,
) -> sqlx::Result<Vec<AwaitingRender>> {
    sqlx::query_as(&format!(
        "SELECT id, CASE preview_status WHEN 'generating'
             THEN json_extract(metadata, '$.preview_model') ELSE thumbnail_path END AS model,
             preview_status = 'generating' AS generating, preview_revision AS revision
         FROM assets WHERE {AWAITING_RENDER} LIMIT ?"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
}

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
/// the searched `metadata`. A GLB left to render is no picture yet, so an asset that has one
/// keeps it, at `generating`, until `save_model_render`. Each GLB bumps `preview_revision`, which
/// drops any render still in flight of the one it replaced.
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
    let keeps_picture = model_path.is_some()
        && rel_thumb.as_deref().is_some_and(|t| t.ends_with(".glb"))
        && previous_thumb
            .as_deref()
            .is_some_and(|t| !t.is_empty() && !t.ends_with(".glb"));
    let (status, rel_thumb) = if keeps_picture {
        ("generating", previous_thumb.clone())
    } else {
        ("done", rel_thumb)
    };
    delete_replaced_thumb(pool, root_path, asset_id, rel_thumb.as_deref()).await;
    if let Some(ref model_path) = model_path {
        let rel_model = make_relative_path(root_path, Path::new(model_path));
        let _ = sqlx::query(
            "UPDATE assets SET preview_status = ?, thumbnail_path = ?, metadata = json_set(coalesce(nullif(metadata, ''), '{}'), '$.preview_model', ?), preview_version = ?, preview_textures = ?, preview_reason = NULL, preview_revision = preview_revision + 1 WHERE id = ?"
        )
        .bind(status)
        .bind(&rel_thumb)
        .bind(&rel_model)
        .bind(report.version)
        .bind(&report.textures)
        .bind(asset_id)
        .execute(pool)
        .await;
    } else {
        let _ = sqlx::query(
            "UPDATE assets SET preview_status = 'done', thumbnail_path = ?, preview_version = ?, preview_textures = ?, preview_reason = NULL WHERE id = ?",
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

/// A saved render: its library-relative path, and the group of the asset it pictures, whose main
/// item's card may show it as a stand-in.
#[derive(Debug, PartialEq)]
pub(crate) struct SavedRender {
    pub rel_thumb: String,
    pub group_id: Option<String>,
}

/// Stores the PNG the frontend rendered from asset `asset_id`'s preview GLB, as listed at
/// `revision`. Each render gets a name of its own, so a card showing the old picture loads the
/// new one rather than the webview's cached copy. A job queued since stays `pending`. Once a job
/// has written a newer GLB the row and its picture stay as they are, and this returns `None`: the
/// render shows an older GLB than the one the row now waits on.
pub(crate) async fn save_model_render(
    pool: &Pool<Sqlite>,
    root_path: &Path,
    asset_id: &str,
    revision: i64,
    png: &[u8],
) -> std::io::Result<Option<SavedRender>> {
    let old: Option<String> = sqlx::query_scalar("SELECT thumbnail_path FROM assets WHERE id = ?")
        .bind(asset_id)
        .fetch_optional(pool)
        .await
        .map_err(std::io::Error::other)?
        .flatten();
    let rel_thumb = format!(
        ".opendam/thumbs/{asset_id}_thumb.{}.png",
        uuid::Uuid::new_v4().simple()
    );
    let abs_thumb = PathBuf::from(resolve_path(root_path, &rel_thumb));
    tokio::fs::write(&abs_thumb, png).await?;
    let group_id: Option<Option<String>> = sqlx::query_scalar(
        "UPDATE assets SET thumbnail_path = ?,
            preview_status = CASE preview_status WHEN 'generating' THEN 'done' ELSE preview_status END
         WHERE id = ? AND preview_revision = ? RETURNING group_id",
    )
    .bind(&rel_thumb)
    .bind(asset_id)
    .bind(revision)
    .fetch_optional(pool)
    .await
    .map_err(std::io::Error::other)?;
    let Some(group_id) = group_id else {
        remove_thumb_file(&abs_thumb).await;
        return Ok(None);
    };
    if let Some(old) = old.and_then(|old| owned_thumb_file(root_path, &old)) {
        remove_thumb_file(&old).await;
    }
    Ok(Some(SavedRender {
        rel_thumb,
        group_id,
    }))
}

/// Moves asset `asset_id` from `generating` to `error` when its GLB, as listed at `revision`,
/// can't be rendered, which frees Regenerate. Its old picture stays. Returns whether the row
/// moved: a first preview, a model queued again since, or one with a newer GLB has nothing to
/// fail.
pub(crate) async fn fail_model_render(
    pool: &Pool<Sqlite>,
    asset_id: &str,
    revision: i64,
) -> sqlx::Result<bool> {
    let moved = sqlx::query(
        "UPDATE assets SET preview_status = 'error'
         WHERE id = ? AND preview_revision = ? AND preview_status = 'generating'",
    )
    .bind(asset_id)
    .bind(revision)
    .execute(pool)
    .await?;
    Ok(moved.rows_affected() > 0)
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

/// A failed job: (job_id, asset_id, why the asset stays without a preview, if it is known)
type FailedJob = (String, String, Option<String>);

async fn fail_job(pool: &Pool<Sqlite>, job_id: &str, asset_id: &str) {
    tracing::error!("Job {} failed", job_id);
    let _ = sqlx::query("UPDATE preview_jobs SET status = 'error' WHERE id = ?")
        .bind(job_id)
        .execute(pool)
        .await;
    let _ = sqlx::query(
        "UPDATE assets SET preview_status = 'error', preview_reason = NULL WHERE id = ?",
    )
    .bind(asset_id)
    .execute(pool)
    .await;
}

/// Ends a job whose file stays without a preview for `reason`, clearing any thumbnail it had, as
/// a job does for a source with none. `error` keeps the asset off the queue, since the structure
/// pass requeues every `none`, until the file changes or you ask again. `reason` goes in
/// `preview_reason`, outside the searched `metadata`.
async fn refuse_job(
    pool: &Pool<Sqlite>,
    root_path: &Path,
    job_id: &str,
    asset_id: &str,
    reason: String,
) -> FailedJob {
    tracing::warn!("Job {} ended without a preview: {}", job_id, reason);
    let _ = sqlx::query("UPDATE preview_jobs SET status = 'error', error_message = ? WHERE id = ?")
        .bind(&reason)
        .bind(job_id)
        .execute(pool)
        .await;
    delete_replaced_thumb(pool, root_path, asset_id, None).await;
    let _ = sqlx::query(
        "UPDATE assets SET preview_status = 'error', thumbnail_path = NULL, preview_reason = ? WHERE id = ?",
    )
    .bind(&reason)
    .bind(asset_id)
    .execute(pool)
    .await;
    (job_id.to_string(), asset_id.to_string(), Some(reason))
}

fn emit_error(state: &AppState, (job_id, asset_id, reason): &FailedJob) {
    let _ = state.app_handle.emit(
        "preview:error",
        json!({
            "assetId": asset_id,
            "jobId": job_id,
            "reason": reason,
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
            let (claimed, done, refused) = process_image_jobs(&lib, parallelism).await;
            processed_any |= claimed;
            for d in &done {
                emit_done(&state, d);
            }
            for r in &refused {
                emit_error(&state, r);
            }
        }

        if !processed_any {
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        }
    }
}

/// Claims and runs one round of `lib`'s image jobs. Returns whether any job was claimed, the
/// jobs that finished, for `preview:done`, and the ones `refuse_job` ended, for `preview:error`.
pub(crate) async fn process_image_jobs(
    lib: &LibraryHandle,
    parallelism: usize,
) -> (bool, Vec<PreviewDone>, Vec<FailedJob>) {
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
    let mut refused = Vec::new();
    for (job_id, asset_id, res) in results {
        match res {
            Ok(ImageThumb::TooLarge(reason)) => {
                refused
                    .push(refuse_job(&lib.pool, &lib.root_path, &job_id, &asset_id, reason).await);
            }
            Ok(thumb) => {
                let rel_thumb = match thumb {
                    ImageThumb::Written(file) => Some(format!(".opendam/thumbs/{file}")),
                    _ => None,
                };
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
    (claimed, done, refused)
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
                finished(Err((job_id, asset_id, None)));
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

        let thumb = generate_image_thumb(&source, &thumbs, "a1").unwrap();

        assert_eq!(thumb, ImageThumb::Original);
        assert!(!thumbs.exists());
    }

    #[test]
    fn small_tga_gets_a_native_size_png() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("sprite.tga");
        write_image(&source, 48, 20);
        let thumbs = dir.path().join("thumbs");

        let thumb = generate_image_thumb(&source, &thumbs, "a1").unwrap();

        assert_eq!(thumb, ImageThumb::Written("a1_thumb.png".into()));
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

        let thumb = generate_image_thumb(&source, &thumbs, "a1").unwrap();

        assert_eq!(thumb, ImageThumb::Written("a1_thumb.png".into()));
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

        assert_eq!(
            generate_image_thumb(&png, &thumbs, "p").unwrap(),
            ImageThumb::Original
        );
        assert_eq!(
            generate_image_thumb(&tif, &thumbs, "t").unwrap(),
            ImageThumb::Written("t_thumb.png".into())
        );
        assert_eq!(
            image::image_dimensions(thumbs.join("t_thumb.png")).unwrap(),
            (512, 100)
        );
    }

    /// Writes a Radiance `.hdr` of `width` x `height` px, `pixels` repeating along each row.
    fn write_hdr(path: &Path, width: u32, height: u32, pixels: &[f32]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let hdr = image::Rgb32FImage::from_fn(width, height, |x, _| {
            image::Rgb([pixels[x as usize % pixels.len()]; 3])
        });
        hdr.save_with_format(path, image::ImageFormat::Hdr).unwrap();
    }

    #[test]
    fn small_hdr_gets_a_tone_mapped_native_size_png() {
        let dir = tempfile::tempdir().unwrap();
        // An upper-case extension, as most real ones have.
        let source = dir.path().join("sky.HDR");
        write_hdr(&source, 4, 2, &[0.0, 0.1, 1.0, 3.0]);
        let thumbs = dir.path().join("thumbs");

        let thumb = generate_image_thumb(&source, &thumbs, "a1").unwrap();
        assert_eq!(thumb, ImageThumb::Written("a1_thumb.png".into()));

        let thumb = image::open(thumbs.join("a1_thumb.png")).unwrap().to_rgb8();
        assert_eq!(thumb.dimensions(), (4, 2));
        // Clipping linear light would give 0, 25, 255 and 255.
        let row: Vec<u8> = (0..4).map(|x| thumb.get_pixel(x, 1).0[0]).collect();
        assert_eq!(row, [0, 85, 186, 223]);
    }

    #[test]
    fn large_hdr_is_downsampled_to_a_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("sky.hdr");
        write_hdr(&source, 1024, 32, &[1.0]);
        let thumbs = dir.path().join("thumbs");

        let thumb = generate_image_thumb(&source, &thumbs, "a1").unwrap();
        assert_eq!(thumb, ImageThumb::Written("a1_thumb.jpg".into()));

        let thumb = image::open(thumbs.join("a1_thumb.jpg")).unwrap().to_rgb8();
        assert_eq!(thumb.dimensions(), (512, 16));
        assert!(thumb.get_pixel(256, 8).0[0].abs_diff(186) <= 2);
    }

    /// A `.hdr` whose header reads but whose pixels stop after its first bytes.
    fn write_truncated_hdr(path: &Path) {
        write_hdr(path, 64, 64, &[0.25, 1.0, 3.0]);
        let bytes = std::fs::read(path).unwrap();
        std::fs::write(path, &bytes[..bytes.len() / 2]).unwrap();
        assert_eq!(image::image_dimensions(path).unwrap(), (64, 64));
    }

    #[test]
    fn truncated_hdr_fails_and_writes_no_thumbnail() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("sky.hdr");
        write_truncated_hdr(&source);
        let thumbs = dir.path().join("thumbs");

        assert!(generate_image_thumb(&source, &thumbs, "a1").is_err());
        assert!(!thumbs.exists());
    }

    #[tokio::test]
    async fn image_queue_hands_a_truncated_hdr_to_the_python_queue() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_truncated_hdr(&root.join("hdri/sky.HDR"));
        let lib = library_with_pending_job(root, "hdri/sky.HDR", "").await;

        let (claimed, done, _) = process_image_jobs(&lib, 2).await;

        assert!(claimed);
        assert!(done.is_empty());
        // Not done: the row keeps waiting, and the worker's answer settles it.
        assert_eq!(
            job_outcome(&lib.pool).await,
            (Some(String::new()), "pending".into(), "fallback".into())
        );
        assert!(!root.join(".opendam/thumbs").exists());

        // The image queue doesn't claim it again.
        let (claimed, ..) = process_image_jobs(&lib, 2).await;
        assert!(!claimed);
    }

    #[test]
    fn hdr_is_decoded_natively_and_exr_is_not() {
        assert!(is_native_image("HDR") && needs_preview("HDR"));
        assert!(!is_native_image("exr") && needs_preview("exr"));
    }

    #[tokio::test]
    async fn image_queue_serves_an_upper_case_hdr_job() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_hdr(&root.join("hdri/sky.HDR"), 8, 4, &[1.0]);
        let lib = library_with_pending_job(root, "hdri/sky.HDR", "").await;

        let (claimed, done, _) = process_image_jobs(&lib, 2).await;

        assert!(claimed);
        assert_eq!(done.len(), 1);
        assert_eq!(
            job_outcome(&lib.pool).await,
            (
                Some(".opendam/thumbs/a1_thumb.png".into()),
                "done".into(),
                "done".into()
            )
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

    /// A PNG whose header claims 29027 x 29027 RGB and which has no pixels: only the header can
    /// be read, so any attempt to decode it fails some other way.
    const HEADER_PAST_THE_LIMIT: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x71, 0x63, 0x00, 0x00, 0x71, 0x63, 0x08, 0x02, 0x00, 0x00, 0x00, 0x75,
        0xea, 0x7f, 0x6e, 0x00, 0x00, 0x00, 0x00, 0x49, 0x44, 0x41, 0x54, 0x35, 0xaf, 0x06, 0x1e,
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    const TOO_LARGE: &str = "Too large to preview (29027 x 29027 px)";
    /// A PNG whose header claims 1024 x 1024 RGB, under the limit and past `THUMB_SIZE`, and which
    /// has no pixels, so its decode fails for a reason other than its size.
    const HEADER_WITH_NO_PIXELS: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x04, 0x00, 0x08, 0x02, 0x00, 0x00, 0x00, 0xf0,
        0x7f, 0xbc, 0xd4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x44, 0x41, 0x54, 0x35, 0xaf, 0x06, 0x1e,
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn raster_past_the_decoders_limit_is_refused_from_its_header() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("huge.png");
        std::fs::write(&source, HEADER_PAST_THE_LIMIT).unwrap();
        let thumbs = dir.path().join("thumbs");

        let thumb = generate_image_thumb(&source, &thumbs, "a1").unwrap();

        assert_eq!(thumb, ImageThumb::TooLarge(TOO_LARGE.into()));
        assert!(!thumbs.exists());
    }

    #[tokio::test]
    async fn a_decode_failure_under_the_limit_is_not_called_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let source = root.join("broken.png");
        std::fs::write(&source, HEADER_WITH_NO_PIXELS).unwrap();
        let thumbs = root.join(".opendam/thumbs");

        assert!(generate_image_thumb(&source, &thumbs, "a1").is_err());

        let lib = library_with_pending_job(root, "broken.png", ".opendam/thumbs/none.png").await;
        let (_, done, refused) = process_image_jobs(&lib, 2).await;

        assert!(done.is_empty());
        assert!(refused.is_empty());
        // The Python queue gets its try, and the asset keeps today's message and Retry.
        let job: String = sqlx::query_scalar("SELECT status FROM preview_jobs WHERE id = 'j1'")
            .fetch_one(&lib.pool)
            .await
            .unwrap();
        assert_eq!(job, "fallback");
        let reason: Option<String> = sqlx::query_scalar("SELECT preview_reason FROM assets")
            .fetch_one(&lib.pool)
            .await
            .unwrap();
        assert_eq!(reason, None);
    }

    #[tokio::test]
    async fn job_for_a_raster_past_the_limit_stores_its_reason_and_clears_its_old_thumbnail() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let old_thumb = root.join(".opendam/thumbs/a1_thumb.png");
        write_image(&old_thumb, 16, 16);
        std::fs::create_dir_all(root.join("textures")).unwrap();
        std::fs::write(root.join("textures/huge.png"), HEADER_PAST_THE_LIMIT).unwrap();
        let lib =
            library_with_pending_job(root, "textures/huge.png", ".opendam/thumbs/a1_thumb.png")
                .await;

        let (claimed, done, refused) = process_image_jobs(&lib, 2).await;

        assert!(claimed);
        assert!(done.is_empty());
        assert_eq!(
            refused,
            [(
                "j1".to_string(),
                "a1".to_string(),
                Some(TOO_LARGE.to_string())
            )]
        );
        assert!(!old_thumb.exists());
        assert_eq!(
            job_outcome(&lib.pool).await,
            (None, "error".into(), "error".into())
        );
        let (reason, metadata): (Option<String>, String) =
            sqlx::query_as("SELECT preview_reason, metadata FROM assets WHERE id = 'a1'")
                .fetch_one(&lib.pool)
                .await
                .unwrap();
        assert_eq!(reason.as_deref(), Some(TOO_LARGE));
        assert_eq!(metadata, "{}");
        // The job is over: nothing is left for a queue to claim, here or as a Python fallback.
        let waiting = assets_waiting_for_jobs(&lib.pool).await.unwrap();
        assert!(waiting.is_empty());
        assert!(!process_image_jobs(&lib, 2).await.0);
    }

    #[tokio::test]
    async fn a_refused_raster_stays_off_the_queue_after_a_structure_pass() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("huge.png"), HEADER_PAST_THE_LIMIT).unwrap();
        let lib = library_with_pending_job(root, "huge.png", ".opendam/thumbs/none.png").await;
        process_image_jobs(&lib, 2).await;

        crate::structure::rebuild(&lib.pool, root).await.unwrap();

        assert_eq!(
            job_outcome(&lib.pool).await,
            (None, "error".into(), "error".into())
        );
        assert!(assets_waiting_for_jobs(&lib.pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_raster_that_shrank_loses_its_reason_when_its_preview_is_done() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_image(&root.join("rock.png"), 256, 256);
        let lib = library_with_pending_job(root, "rock.png", ".opendam/thumbs/none.png").await;
        sqlx::query("UPDATE assets SET preview_reason = 'Too large to preview'")
            .execute(&lib.pool)
            .await
            .unwrap();

        process_image_jobs(&lib, 2).await;

        let reason: Option<String> = sqlx::query_scalar("SELECT preview_reason FROM assets")
            .fetch_one(&lib.pool)
            .await
            .unwrap();
        assert_eq!(reason, None);
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

        let (claimed, done, _) = process_image_jobs(&lib, 2).await;

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

        let (_, done, _) = process_image_jobs(&lib, 2).await;

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

    /// Where a model's job leaves asset `m1`'s GLB.
    const GLB: &str = ".opendam/thumbs/m1_preview.glb";

    /// A model's job whose GLB is left to render, as `python_queue_round` finishes it.
    async fn finish_with_glb(pool: &Pool<Sqlite>, root: &Path, job: &str) -> PreviewDone {
        complete_job(
            pool,
            root,
            job,
            "m1",
            Some(GLB.to_string()),
            Some(root.join(GLB).to_string_lossy().into_owned()),
            &EngineReport::default(),
        )
        .await
    }

    /// Asset `m1`'s current revision.
    async fn revision(pool: &Pool<Sqlite>) -> i64 {
        sqlx::query_scalar("SELECT preview_revision FROM assets WHERE id = 'm1'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Asset `m1`'s render of its current GLB, saved.
    async fn save_render(pool: &Pool<Sqlite>, root: &Path, png: &[u8]) -> SavedRender {
        save_model_render(pool, root, "m1", revision(pool).await, png)
            .await
            .unwrap()
            .expect("no newer GLB")
    }

    /// The (id, model, generating) of up to `limit` assets awaiting a render.
    async fn awaiting(pool: &Pool<Sqlite>, limit: i64) -> Vec<(String, String, bool)> {
        let rows = models_awaiting_render(pool, limit).await.unwrap();
        rows.into_iter()
            .map(|r| (r.id, r.model, r.generating))
            .collect()
    }

    async fn thumb_and_status(pool: &Pool<Sqlite>) -> (Option<String>, String) {
        sqlx::query_as("SELECT thumbnail_path, preview_status FROM assets WHERE id = 'm1'")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_rerun_model_keeps_its_picture_until_its_render_is_saved() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let thumbs = root.join(".opendam/thumbs");
        std::fs::create_dir_all(&thumbs).unwrap();
        let old = ".opendam/thumbs/m1_thumb.png";
        std::fs::write(root.join(old), "old").unwrap();
        let pool = crate::db::memory_library_pool().await;
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes,
                modified_at, indexed_at, thumbnail_path, preview_status)
             VALUES ('m1', 'm.glb', 'm.glb', 'glb', '3d_model', 1, 0, 0, ?, 'pending')",
        )
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();

        let done = finish_with_glb(&pool, root, "j1").await;

        assert_eq!(
            done.thumb_path.as_deref(),
            root.join(old).to_str(),
            "the card keeps its picture"
        );
        assert_eq!(
            thumb_and_status(&pool).await,
            (Some(old.to_string()), "generating".to_string())
        );
        assert!(root.join(old).is_file());
        assert_eq!(
            awaiting(&pool, 10).await,
            [("m1".to_string(), GLB.to_string(), true)],
            "the thumbnailer renders the GLB, not the old picture"
        );

        let new = save_render(&pool, root, b"new").await.rel_thumb;

        assert_eq!(awaiting(&pool, 10).await, []);
        assert_ne!(new, old, "a new URL, so the webview loads the new picture");
        assert_eq!(
            thumb_and_status(&pool).await,
            (Some(new.clone()), "done".to_string())
        );
        assert_eq!(std::fs::read(root.join(&new)).unwrap(), b"new");
        assert!(!root.join(old).exists(), "the old picture goes with it");
    }

    /// A library holding only asset `m1`, a model waiting on its first preview.
    async fn library_with_new_model() -> (tempfile::TempDir, Pool<Sqlite>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".opendam/thumbs")).unwrap();
        let pool = crate::db::memory_library_pool().await;
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes,
                modified_at, indexed_at, preview_status)
             VALUES ('m1', 'm.glb', 'm.glb', 'glb', '3d_model', 1, 0, 0, 'pending')",
        )
        .execute(&pool)
        .await
        .unwrap();
        (dir, pool)
    }

    /// The names in the library's `.opendam/thumbs`, sorted.
    fn thumb_files(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(root.join(".opendam/thumbs"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn a_first_preview_holds_its_glb_until_its_render_replaces_it_and_keeps_the_file() {
        let (dir, pool) = library_with_new_model().await;
        let root = dir.path();
        std::fs::write(root.join(GLB), "glb").unwrap();

        finish_with_glb(&pool, root, "j1").await;

        assert_eq!(
            thumb_and_status(&pool).await,
            (Some(GLB.to_string()), "done".to_string())
        );

        let new = save_render(&pool, root, b"new").await.rel_thumb;

        assert_eq!(
            thumb_and_status(&pool).await,
            (Some(new), "done".to_string())
        );
        assert!(root.join(GLB).is_file(), "the 3D viewer still loads it");
    }

    #[tokio::test]
    async fn a_second_render_replaces_the_first_by_a_new_name() {
        let (dir, pool) = library_with_new_model().await;
        let root = dir.path();

        let first = save_render(&pool, root, b"one").await;
        let second = save_render(&pool, root, b"two").await;
        let (first, second) = (first.rel_thumb, second.rel_thumb);

        assert_ne!(first, second);
        let (_, name) = second.rsplit_once('/').unwrap();
        assert_eq!(thumb_files(root), [name], "the first render is deleted");
        assert_eq!(std::fs::read(root.join(&second)).unwrap(), b"two");
    }

    /// The thumbnailer's selection gets a GLB by the row's state, not by whichever column is set:
    /// here `m1` is a first preview and `m2` a re-render, with both columns telling them apart.
    #[tokio::test]
    async fn the_thumbnailer_gets_a_first_previews_thumbnail_and_a_re_renders_model() {
        let pool = crate::db::memory_library_pool().await;
        for (id, thumb, status, metadata) in [
            ("m1", "held.glb", "done", r#"{"preview_model":"other.glb"}"#),
            (
                "m2",
                "old.png",
                "generating",
                r#"{"preview_model":"new.glb"}"#,
            ),
            ("m3", "own.png", "done", r#"{"preview_model":"own.glb"}"#),
        ] {
            sqlx::query(
                "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes,
                    modified_at, indexed_at, thumbnail_path, preview_status, metadata)
                 VALUES (?1, ?1, ?1, 'glb', '3d_model', 1, 0, 0, ?2, ?3, ?4)",
            )
            .bind(id)
            .bind(thumb)
            .bind(status)
            .bind(metadata)
            .execute(&pool)
            .await
            .unwrap();
        }

        let mut listed = awaiting(&pool, 10).await;
        listed.sort();

        assert_eq!(
            listed,
            [
                ("m1".to_string(), "held.glb".to_string(), false),
                ("m2".to_string(), "new.glb".to_string(), true),
            ]
        );
        assert_eq!(awaiting(&pool, 1).await.len(), 1);
    }

    #[tokio::test]
    async fn a_saved_render_names_the_group_whose_card_may_show_it() {
        let (dir, pool) = library_with_new_model().await;

        let alone = save_render(&pool, dir.path(), b"png").await;
        assert_eq!(alone.group_id, None);

        sqlx::query("UPDATE assets SET group_id = 'main'")
            .execute(&pool)
            .await
            .unwrap();
        let member = save_render(&pool, dir.path(), b"png").await;
        assert_eq!(member.group_id.as_deref(), Some("main"));
    }

    #[tokio::test]
    async fn the_thumbnailer_gets_each_model_with_its_revision() {
        let (_dir, pool) = library_with_new_model().await;
        sqlx::query("UPDATE assets SET thumbnail_path = 'held.glb', preview_revision = 7")
            .execute(&pool)
            .await
            .unwrap();

        let listed = models_awaiting_render(&pool, 10).await.unwrap();

        assert_eq!(listed[0].revision, 7);
    }

    /// `m1` at `generating`, showing `old`, at revision 2: the thumbnailer listed job A's GLB at
    /// revision 1, and job B replaced it before that render came back. Nothing else on the row
    /// changes, as when the model changed before A finished.
    async fn changed_during_its_render(pool: &Pool<Sqlite>, root: &Path, old: &str) {
        std::fs::write(root.join(old), "old").unwrap();
        sqlx::query("UPDATE assets SET thumbnail_path = ?")
            .bind(old)
            .execute(pool)
            .await
            .unwrap();
        finish_with_glb(pool, root, "j1").await;
        assert_eq!(revision(pool).await, 1);
        finish_with_glb(pool, root, "j2").await;
        assert_eq!(revision(pool).await, 2);
    }

    #[tokio::test]
    async fn a_render_of_an_older_glb_leaves_the_newer_one_waiting() {
        let (dir, pool) = library_with_new_model().await;
        let root = dir.path();
        let old = ".opendam/thumbs/m1_thumb.png";
        changed_during_its_render(&pool, root, old).await;

        let saved = save_model_render(&pool, root, "m1", 1, b"older").await;

        assert_eq!(saved.unwrap(), None);
        assert_eq!(
            thumb_and_status(&pool).await,
            (Some(old.to_string()), "generating".to_string())
        );
        assert_eq!(thumb_files(root), ["m1_thumb.png"], "the render is dropped");
        assert_eq!(
            awaiting(&pool, 10).await,
            [("m1".to_string(), GLB.to_string(), true)]
        );
    }

    #[tokio::test]
    async fn a_failed_render_of_an_older_glb_leaves_the_newer_one_waiting() {
        let (dir, pool) = library_with_new_model().await;
        let old = ".opendam/thumbs/m1_thumb.png";
        changed_during_its_render(&pool, dir.path(), old).await;

        assert!(!fail_model_render(&pool, "m1", 1).await.unwrap());

        assert_eq!(
            thumb_and_status(&pool).await,
            (Some(old.to_string()), "generating".to_string())
        );
    }

    #[tokio::test]
    async fn a_render_of_a_first_preview_that_changed_keeps_its_held_glb() {
        let (dir, pool) = library_with_new_model().await;
        let root = dir.path();
        finish_with_glb(&pool, root, "j1").await;
        finish_with_glb(&pool, root, "j2").await;

        let saved = save_model_render(&pool, root, "m1", 1, b"older").await;

        assert_eq!(saved.unwrap(), None);
        assert_eq!(
            thumb_and_status(&pool).await,
            (Some(GLB.to_string()), "done".to_string())
        );
    }

    #[tokio::test]
    async fn a_failed_render_leaves_the_model_retriable_with_its_picture() {
        let (dir, pool) = library_with_new_model().await;
        let root = dir.path();
        let old = ".opendam/thumbs/m1_thumb.png";
        sqlx::query("UPDATE assets SET thumbnail_path = ?")
            .bind(old)
            .execute(&pool)
            .await
            .unwrap();
        finish_with_glb(&pool, root, "j1").await;

        assert!(fail_model_render(&pool, "m1", 1).await.unwrap());

        assert_eq!(
            thumb_and_status(&pool).await,
            (Some(old.to_string()), "error".to_string())
        );
        assert_eq!(awaiting(&pool, 10).await, []);
    }

    #[tokio::test]
    async fn only_a_model_at_generating_fails_its_render() {
        let (dir, pool) = library_with_new_model().await;

        assert!(!fail_model_render(&pool, "m1", 0).await.unwrap(), "pending");

        finish_with_glb(&pool, dir.path(), "j1").await;

        assert!(!fail_model_render(&pool, "m1", 1).await.unwrap(), "first");
        assert_eq!(
            thumb_and_status(&pool).await,
            (Some(GLB.to_string()), "done".to_string())
        );
    }

    #[tokio::test]
    async fn a_render_finishing_after_the_model_changed_leaves_it_pending() {
        let (dir, pool) = library_with_new_model().await;

        let new = save_render(&pool, dir.path(), b"new").await.rel_thumb;

        assert_eq!(
            thumb_and_status(&pool).await,
            (Some(new), "pending".to_string())
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

        // A re-preview with no thumbnail leaves the model to render, and the old picture stands.
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
        assert!(old_thumb.exists());
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
        assert!(!old_thumb.exists());
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
    async fn a_failed_job_clears_a_stale_preview_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = stub_engine(dir.path(), ANSWER_2, "exit 1");
        let lib = library_with_pending_job(dir.path(), "models/ship.fbx", "").await;
        sqlx::query("UPDATE assets SET preview_reason = 'Too large to preview'")
            .execute(&lib.pool)
            .await
            .unwrap();

        let outcomes = queue_round(&mut engine, std::slice::from_ref(&lib)).await;

        assert!(
            matches!(&outcomes[..], [Err((_, _, None))]),
            "the job fails with no reason: {outcomes:?}"
        );
        let (status, reason): (String, Option<String>) =
            sqlx::query_as("SELECT preview_status, preview_reason FROM assets WHERE id = 'a1'")
                .fetch_one(&lib.pool)
                .await
                .unwrap();
        assert_eq!((status.as_str(), reason), ("error", None));
    }

    #[tokio::test]
    async fn a_job_that_kills_the_worker_unpublishes_and_a_new_one_starts_at_once() {
        let dir = tempfile::tempdir().unwrap();
        // Every process exits on its job, as a crash does
        let mut engine = stub_engine(dir.path(), ANSWER_2, "exit 1");
        let lib = library_with_pending_job(dir.path(), "models/ship.fbx", "").await;

        let outcomes = queue_round(&mut engine, std::slice::from_ref(&lib)).await;
        assert!(
            matches!(&outcomes[..], [Err((job, asset, None))] if job == "j1" && asset == "a1"),
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
