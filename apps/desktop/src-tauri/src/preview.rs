use crate::db::{make_relative_path, resolve_path};
use crate::AppState;
use anyhow::{anyhow, Result};
use image::imageops::FilterType;
use serde_json::json;
use sqlx::{Pool, Sqlite};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::Emitter;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub const THUMB_SIZE: u32 = 512;

/// Checks if an extension is a standard raster image that Rust can decode directly.
pub fn is_native_image(ext: &str) -> bool {
    matches!(
        ext.to_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "bmp" | "gif" | "tif" | "tiff" | "tga" | "webp"
    )
}

/// Blazingly fast in-process image downsampling using the native Rust `image` crate.
/// Uses Triangle (bilinear) filtering for 4x–8x faster performance than Lanczos3.
pub fn generate_image_thumb(source: &Path, output: &Path) -> Result<()> {
    let img = image::open(source)?;
    let thumb = img.resize(THUMB_SIZE, THUMB_SIZE, FilterType::Triangle);

    if let Some(parent) = output.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let is_jpg = output
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.eq_ignore_ascii_case("jpg") || s.eq_ignore_ascii_case("jpeg"))
        .unwrap_or(false);

    if is_jpg {
        let rgb = thumb.to_rgb8();
        rgb.save_with_format(output, image::ImageFormat::Jpeg)?;
    } else {
        thumb.save(output)?;
    }
    Ok(())
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
            .stderr(std::process::Stdio::null())
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

        let mut line = String::new();
        loop {
            line.clear();
            let bytes_read = self.reader.read_line(&mut line).await?;
            if bytes_read == 0 {
                return Err(anyhow!("Python worker closed stdout unexpectedly"));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with('{') {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    return Ok(val);
                }
            }
        }
    }

    async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

pub fn start_preview_worker(state: Arc<AppState>) {
    tauri::async_runtime::spawn(async move {
        // Dynamically resolve python worker and script paths
        let current_dir = std::env::current_dir().unwrap_or_default();
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| current_dir.clone());

        let mut search_roots = vec![current_dir.clone(), exe_dir.clone()];
        let mut curr = current_dir.clone();
        for _ in 0..4 {
            if let Some(parent) = curr.parent() {
                search_roots.push(parent.to_path_buf());
                curr = parent.to_path_buf();
            } else {
                break;
            }
        }
        let mut curr_exe = exe_dir.clone();
        for _ in 0..4 {
            if let Some(parent) = curr_exe.parent() {
                search_roots.push(parent.to_path_buf());
                curr_exe = parent.to_path_buf();
            } else {
                break;
            }
        }

        let py_exec = if let Ok(custom) = std::env::var("OPENDAM_PYTHON") {
            PathBuf::from(custom)
        } else {
            let mut found_py = None;
            for root in &search_roots {
                let candidates = [
                    root.join("packages")
                        .join("preview-engine")
                        .join(".venv")
                        .join("bin")
                        .join("python"),
                    root.join("packages")
                        .join("preview-engine")
                        .join("venv")
                        .join("bin")
                        .join("python"),
                    root.join("venv").join("bin").join("python"),
                ];
                if let Some(p) = candidates.into_iter().find(|p| p.exists()) {
                    found_py = Some(p);
                    break;
                }
            }
            found_py.unwrap_or_else(|| PathBuf::from("python3"))
        };

        let worker_py = if let Ok(custom) = std::env::var("OPENDAM_WORKER") {
            PathBuf::from(custom)
        } else {
            let mut found_worker = None;
            for root in &search_roots {
                let candidates = [
                    root.join("packages")
                        .join("preview-engine")
                        .join("worker.py"),
                    root.join("worker.py"),
                ];
                if let Some(p) = candidates.into_iter().find(|p| p.exists()) {
                    found_worker = Some(p);
                    break;
                }
            }
            found_worker.unwrap_or_else(|| PathBuf::from("worker.py"))
        };

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
        run_python_queue(state, py_exec, worker_py).await;
    });
}

/// SQL list of the extensions `is_native_image` accepts, matched against `assets.extension`.
const NATIVE_EXTS_SQL: &str = "('jpg','jpeg','png','bmp','gif','tif','tiff','tga','webp')";

/// A claimed job: (job_id, asset_id, file_path)
type Job = (String, String, String);

/// Fetches up to `limit` jobs matching `filter` and marks them as 'processing'.
async fn claim_jobs(pool: &Pool<Sqlite>, filter: &str, limit: usize) -> Vec<Job> {
    let jobs: Vec<Job> = sqlx::query_as(&format!(
        "SELECT p.id, p.asset_id, a.file_path FROM preview_jobs p JOIN assets a ON p.asset_id = a.id WHERE ({}) LIMIT {}",
        filter, limit
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

/// Marks a job done, stores the asset's preview paths and notifies the UI.
/// `rel_thumb` is relative to the library root; `model_path` is absolute.
async fn complete_job(
    state: &AppState,
    pool: &Pool<Sqlite>,
    root_path: &Path,
    job_id: &str,
    asset_id: &str,
    rel_thumb: Option<String>,
    model_path: Option<String>,
) {
    let _ = sqlx::query("UPDATE preview_jobs SET status = 'done' WHERE id = ?")
        .bind(job_id)
        .execute(pool)
        .await;

    if let Some(ref model_path) = model_path {
        let rel_model = make_relative_path(root_path, Path::new(model_path));
        let _ = sqlx::query(
            "UPDATE assets SET preview_status = 'done', thumbnail_path = ?, metadata = json_set(coalesce(nullif(metadata, ''), '{}'), '$.preview_model', ?) WHERE id = ?"
        )
        .bind(&rel_thumb)
        .bind(&rel_model)
        .bind(asset_id)
        .execute(pool)
        .await;
    } else {
        let _ = sqlx::query("UPDATE assets SET preview_status = 'done', thumbnail_path = COALESCE(?, thumbnail_path) WHERE id = ?")
            .bind(&rel_thumb)
            .bind(asset_id)
            .execute(pool)
            .await;
    }

    let resolved_thumb = rel_thumb.as_ref().map(|r| resolve_path(root_path, r));
    let _ = state.app_handle.emit(
        "preview:done",
        json!({
            "assetId": asset_id,
            "thumbnailPath": &resolved_thumb,
            "previewModel": &model_path,
        }),
    );
}

async fn fail_job(state: &AppState, pool: &Pool<Sqlite>, job_id: &str, asset_id: &str) {
    tracing::error!("Job {} failed", job_id);
    let _ = sqlx::query("UPDATE preview_jobs SET status = 'error' WHERE id = ?")
        .bind(job_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("UPDATE assets SET preview_status = 'error' WHERE id = ?")
        .bind(asset_id)
        .execute(pool)
        .await;
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
    let filter = format!(
        "p.status IN ('queued', 'pending') AND lower(coalesce(a.extension, '')) IN {}",
        NATIVE_EXTS_SQL
    );

    loop {
        let mut processed_any = false;

        for lib in state.db.get_all_libraries().await {
            let thumb_dir = lib.root_path.join(".opendam").join("thumbs");
            let _ = tokio::fs::create_dir_all(&thumb_dir).await;

            let jobs = claim_jobs(&lib.pool, &filter, parallelism * 8).await;
            processed_any |= !jobs.is_empty();

            let mut tasks = tokio::task::JoinSet::new();
            let mut results = Vec::new();
            for (job_id, asset_id, file_path) in jobs {
                let source = resolve_source(&lib.root_path, &file_path);
                let ext = source
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                // Named by asset id: file names repeat across folders, asset ids don't.
                let thumb_file_name = if ext == "png" || ext == "webp" {
                    format!("{}_thumb.png", asset_id)
                } else {
                    format!("{}_thumb.jpg", asset_id)
                };
                let dest = thumb_dir.join(&thumb_file_name);

                tasks.spawn_blocking(move || {
                    // A malformed file can panic inside the decoder; treat that as a failure.
                    let res = std::panic::catch_unwind(|| generate_image_thumb(&source, &dest))
                        .unwrap_or_else(|_| Err(anyhow!("image decoder panicked")))
                        .map_err(|e| format!("{}: {}", source.display(), e));
                    (job_id, asset_id, thumb_file_name, res)
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

            for (job_id, asset_id, thumb_file_name, res) in results {
                match res {
                    Ok(()) => {
                        let rel_thumb = format!(".opendam/thumbs/{}", thumb_file_name);
                        complete_job(
                            &state,
                            &lib.pool,
                            &lib.root_path,
                            &job_id,
                            &asset_id,
                            Some(rel_thumb),
                            None,
                        )
                        .await;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Native Rust image thumbnail failed for {}. Falling back to Python.",
                            e
                        );
                        let _ =
                            sqlx::query("UPDATE preview_jobs SET status = 'fallback' WHERE id = ?")
                                .bind(&job_id)
                                .execute(&lib.pool)
                                .await;
                    }
                }
            }
        }

        if !processed_any {
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        }
    }
}

/// Runs 3D/CAD/DCC formats (and images Rust couldn't decode) through the persistent Python worker.
async fn run_python_queue(state: Arc<AppState>, py_exec: PathBuf, worker_py: PathBuf) {
    let filter = format!(
        "(p.status IN ('queued', 'pending') AND lower(coalesce(a.extension, '')) NOT IN {}) OR p.status = 'fallback'",
        NATIVE_EXTS_SQL
    );
    let mut py_worker: Option<PythonWorker> = None;

    loop {
        let mut processed_any = false;

        for lib in state.db.get_all_libraries().await {
            let pool = &lib.pool;
            let root_path = &lib.root_path;
            let thumb_dir = root_path.join(".opendam").join("thumbs");
            let _ = tokio::fs::create_dir_all(&thumb_dir).await;

            for (job_id, asset_id, file_path) in claim_jobs(pool, &filter, 10).await {
                processed_any = true;
                let full_source = resolve_source(root_path, &file_path);

                if py_worker.is_none() {
                    match PythonWorker::spawn(&py_exec, &worker_py).await {
                        Ok(w) => py_worker = Some(w),
                        Err(e) => {
                            tracing::error!("Failed to spawn Python preview worker: {}", e);
                        }
                    }
                }

                let req = json!({
                    "id": &job_id,
                    "method": "model_meta",
                    "params": {
                        "source": full_source.to_string_lossy().to_string(),
                        "output_dir": thumb_dir.to_string_lossy().to_string(),
                        "name": &asset_id
                    }
                });

                let py_resp = if let Some(ref mut worker) = py_worker {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(60),
                        worker.execute(&req),
                    )
                    .await
                    {
                        Ok(Ok(val)) => Ok(val),
                        Ok(Err(e)) => {
                            tracing::error!(
                                "Python worker execute error: {}. Restarting worker.",
                                e
                            );
                            worker.kill().await;
                            py_worker = None;
                            Err(e)
                        }
                        Err(_) => {
                            tracing::error!(
                                "Python worker timed out after 60s processing {}. Killing worker.",
                                full_source.display()
                            );
                            worker.kill().await;
                            py_worker = None;
                            Err(anyhow!("Job timed out"))
                        }
                    }
                } else {
                    Err(anyhow!("Python worker unavailable"))
                };

                let mut success = false;
                let mut result_thumb = None;
                let mut result_model = None;

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
                    complete_job(
                        &state,
                        pool,
                        root_path,
                        &job_id,
                        &asset_id,
                        rel_thumb,
                        result_model,
                    )
                    .await;
                } else {
                    fail_job(&state, pool, &job_id, &asset_id).await;
                }

                // Yield execution between items to allow other async runtime tasks to progress
                tokio::task::yield_now().await;
            }
        }

        if !processed_any {
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        }
    }
}
