//! A one-time pass over each library that removes the upscaled thumbnails made before
//! thumbnails stopped being upscaled. A raster 512 px or smaller that the webview displays
//! loses its thumbnail, so the UI shows the original; a small TGA or TIFF is re-queued, so the
//! image queue rewrites its thumbnail at native size. `library_meta` records the pass, so later
//! opens read no image headers.

use crate::db::{resolve_path, LibraryHandle};
use crate::preview::{
    catch_decoder_panic, is_webview_displayable, owned_thumb_file, remove_thumb_file,
    NATIVE_EXTS_SQL, THUMB_SIZE,
};
use crate::AppState;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::Emitter;
use uuid::Uuid;

const META_KEY: &str = "thumbs_no_upscale";
const BATCH_SIZE: usize = 200;
const ACTIVE_JOB_STATUSES: &str = "('pending', 'queued', 'processing', 'fallback')";

/// How a call to `run_pass` ended.
#[derive(Debug, PartialEq)]
pub(crate) enum Pass {
    /// `library_meta` already recorded it, so nothing was read.
    AlreadyDone,
    Finished,
    /// The test hook stopped it before a batch's commit.
    Interrupted,
}

/// Runs the pass in the background for every open library, or only `library_id`, and emits
/// `library:updated` once for each library it finishes.
pub fn spawn(state: Arc<AppState>, library_id: Option<String>) {
    tauri::async_runtime::spawn(async move {
        for lib in state.db.get_all_libraries().await {
            if library_id.as_ref().is_some_and(|id| *id != lib.id) {
                continue;
            }
            match run_pass(&lib, BATCH_SIZE, None).await {
                Ok(Pass::Finished) => {
                    let _ = state.app_handle.emit("library:updated", ());
                }
                Ok(_) => {}
                Err(e) => tracing::error!("Thumbnail cleanup of library {} failed: {}", lib.id, e),
            }
        }
    });
}

/// An asset with a thumbnail: (id, file_path, lowercase extension, thumbnail_path).
type Candidate = (String, String, String, String);

enum Action {
    /// Delete the thumbnail file and clear `thumbnail_path`.
    Clear,
    /// Queue a preview job, which rewrites the thumbnail at native size.
    Requeue,
}

/// Runs the pass over `lib` unless `library_meta` records it. Each batch's thumbnail files are
/// deleted before the batch's rows are committed, so an interrupted pass leaves rows pointing at
/// missing files (the UI shows the original), and the next open redoes that batch. Rows that were
/// cleared no longer match, and a small TGA or TIFF is skipped while it has an active job or once
/// its thumbnail has the source's size, so a rerun never queues one twice. `stop_before_commit`
/// is a test hook that stops the pass after batch `n`'s files are deleted.
pub(crate) async fn run_pass(
    lib: &LibraryHandle,
    batch_size: usize,
    stop_before_commit: Option<usize>,
) -> Result<Pass> {
    let done: Option<String> = sqlx::query_scalar("SELECT value FROM library_meta WHERE key = ?")
        .bind(META_KEY)
        .fetch_optional(&lib.pool)
        .await?;
    if done.as_deref() == Some("done") {
        return Ok(Pass::AlreadyDone);
    }

    // Only rasters: a model's thumbnail is also named `<id>_thumb.png`. An asset with an active
    // job is left to it, since the job applies the size rule and deletes the old file itself.
    let candidates: Vec<Candidate> = sqlx::query_as(&format!(
        "SELECT a.id, a.file_path, lower(a.extension), a.thumbnail_path FROM assets a
         WHERE a.thumbnail_path IS NOT NULL AND lower(a.extension) IN {NATIVE_EXTS_SQL}
         AND NOT EXISTS (SELECT 1 FROM preview_jobs p
                         WHERE p.asset_id = a.id AND p.status IN {ACTIVE_JOB_STATUSES})
         ORDER BY a.rowid"
    ))
    .fetch_all(&lib.pool)
    .await?;

    let (mut cleared, mut requeued) = (0, 0);
    for (batch_no, batch) in candidates.chunks(batch_size).enumerate() {
        let root = lib.root_path.clone();
        let batch = batch.to_vec();
        let actions = tokio::task::spawn_blocking(move || {
            batch
                .into_iter()
                .filter_map(|c| classify(&root, &c).map(|a| (c, a)))
                .collect::<Vec<_>>()
        })
        .await?;

        for (c, action) in &actions {
            if let (Action::Clear, Some(file)) = (action, owned_thumb_file(&lib.root_path, &c.3)) {
                remove_thumb_file(&file).await;
            }
        }
        if stop_before_commit == Some(batch_no) {
            return Ok(Pass::Interrupted);
        }

        let now = chrono::Utc::now().timestamp_millis();
        let mut tx = lib.pool.begin().await?;
        for ((id, _, _, thumb), action) in &actions {
            match action {
                Action::Clear => {
                    // Unless a job changed the row since it was read.
                    sqlx::query(
                        "UPDATE assets SET thumbnail_path = NULL WHERE id = ? AND thumbnail_path = ?",
                    )
                    .bind(id)
                    .bind(thumb)
                    .execute(&mut *tx)
                    .await?;
                    cleared += 1;
                }
                Action::Requeue => {
                    sqlx::query("UPDATE assets SET preview_status = 'pending' WHERE id = ?")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query(&format!(
                        "INSERT INTO preview_jobs (id, asset_id, status, created_at)
                         SELECT ?, ?, 'pending', ? WHERE NOT EXISTS (
                             SELECT 1 FROM preview_jobs WHERE asset_id = ?
                             AND status IN {ACTIVE_JOB_STATUSES})"
                    ))
                    .bind(Uuid::new_v4().to_string())
                    .bind(id)
                    .bind(now)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
                    requeued += 1;
                }
            }
        }
        tx.commit().await?;
    }

    sqlx::query(
        "INSERT INTO library_meta (key, value) VALUES (?, 'done')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(META_KEY)
    .execute(&lib.pool)
    .await?;
    tracing::info!(
        "Thumbnail cleanup of library {}: {} cleared, {} re-queued, of {} checked",
        lib.id,
        cleared,
        requeued,
        candidates.len()
    );
    Ok(Pass::Finished)
}

/// What the pass does with one candidate, from image headers alone. Sources larger than
/// `THUMB_SIZE`, missing or unreadable get nothing, nor does a small TGA or TIFF whose thumbnail
/// already has its size.
fn classify(root: &Path, (_, file_path, ext, thumb): &Candidate) -> Option<Action> {
    let dimensions = |rel: &str| {
        let path = PathBuf::from(resolve_path(root, rel));
        catch_decoder_panic(|| Ok(image::image_dimensions(&path)?)).ok()
    };
    let (width, height) = dimensions(file_path)?;
    if width.max(height) > THUMB_SIZE {
        return None;
    }
    if is_webview_displayable(ext) {
        return Some(Action::Clear);
    }
    (dimensions(thumb) != Some((width, height))).then_some(Action::Requeue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{Pool, Sqlite};

    fn write_image(path: &Path, width: u32, height: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::RgbImage::new(width, height).save(path).unwrap();
    }

    async fn add_asset(pool: &Pool<Sqlite>, id: &str, file_path: &str, thumb: &str) {
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, thumbnail_path, preview_status)
             VALUES (?, ?, ?, ?, 'texture', 0, 0, 0, ?, 'done')",
        )
        .bind(id)
        .bind(file_path)
        .bind(file_path.rsplit('/').next().unwrap())
        .bind(file_path.rsplit('.').next().unwrap())
        .bind(thumb)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A library at `root` holding, in this order: a small TGA with an upscaled JPEG thumbnail,
    /// three small PNGs with upscaled thumbnails, a large PNG, a model and a PNG whose source is
    /// missing, each with a thumbnail.
    async fn library(root: &Path) -> LibraryHandle {
        let pool = crate::db::memory_library_pool().await;
        let thumbs = root.join(".opendam/thumbs");

        write_image(&root.join("tex/t1.tga"), 64, 32);
        write_image(&thumbs.join("t1_thumb.jpg"), 512, 256);
        add_asset(&pool, "t1", "tex/t1.tga", ".opendam/thumbs/t1_thumb.jpg").await;
        for id in ["s1", "s2", "s3"] {
            write_image(&root.join(format!("tex/{id}.png")), 32, 32);
            write_image(&thumbs.join(format!("{id}_thumb.png")), 64, 64);
            let thumb = format!(".opendam/thumbs/{id}_thumb.png");
            add_asset(&pool, id, &format!("tex/{id}.png"), &thumb).await;
        }
        write_image(&root.join("tex/big.png"), 1024, 16);
        write_image(&thumbs.join("big_thumb.png"), 512, 8);
        add_asset(&pool, "big", "tex/big.png", ".opendam/thumbs/big_thumb.png").await;
        std::fs::write(root.join("tex/ship.glb"), "glb").unwrap();
        write_image(&thumbs.join("m1_thumb.png"), 64, 64);
        add_asset(&pool, "m1", "tex/ship.glb", ".opendam/thumbs/m1_thumb.png").await;
        write_image(&thumbs.join("gone_thumb.png"), 64, 64);
        add_asset(
            &pool,
            "gone",
            "tex/gone.png",
            ".opendam/thumbs/gone_thumb.png",
        )
        .await;

        LibraryHandle {
            id: "lib".into(),
            name: "lib".into(),
            root_path: root.to_path_buf(),
            pool,
        }
    }

    /// Rows as (id, thumbnail_path, preview_status), file names under `.opendam/thumbs`, and
    /// jobs as (asset_id, status), each sorted.
    async fn snapshot(
        lib: &LibraryHandle,
    ) -> (
        Vec<(String, Option<String>, String)>,
        Vec<String>,
        Vec<(String, String)>,
    ) {
        let rows =
            sqlx::query_as("SELECT id, thumbnail_path, preview_status FROM assets ORDER BY id")
                .fetch_all(&lib.pool)
                .await
                .unwrap();
        let mut files: Vec<String> = std::fs::read_dir(lib.root_path.join(".opendam/thumbs"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        let jobs = sqlx::query_as("SELECT asset_id, status FROM preview_jobs ORDER BY asset_id")
            .fetch_all(&lib.pool)
            .await
            .unwrap();
        (rows, files, jobs)
    }

    async fn meta(lib: &LibraryHandle) -> Option<String> {
        sqlx::query_scalar("SELECT value FROM library_meta WHERE key = 'thumbs_no_upscale'")
            .fetch_optional(&lib.pool)
            .await
            .unwrap()
    }

    fn row(id: &str, thumb: Option<&str>, status: &str) -> (String, Option<String>, String) {
        (id.into(), thumb.map(Into::into), status.into())
    }

    #[tokio::test]
    async fn clears_small_rasters_requeues_small_tgas_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path()).await;

        assert_eq!(run_pass(&lib, 2, None).await.unwrap(), Pass::Finished);

        let (rows, files, jobs) = snapshot(&lib).await;
        assert_eq!(
            rows,
            vec![
                row("big", Some(".opendam/thumbs/big_thumb.png"), "done"),
                row("gone", Some(".opendam/thumbs/gone_thumb.png"), "done"),
                row("m1", Some(".opendam/thumbs/m1_thumb.png"), "done"),
                row("s1", None, "done"),
                row("s2", None, "done"),
                row("s3", None, "done"),
                row("t1", Some(".opendam/thumbs/t1_thumb.jpg"), "pending"),
            ]
        );
        assert_eq!(
            files,
            vec![
                "big_thumb.png",
                "gone_thumb.png",
                "m1_thumb.png",
                "t1_thumb.jpg"
            ]
        );
        assert_eq!(jobs, vec![("t1".to_string(), "pending".to_string())]);
        assert_eq!(meta(&lib).await.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn a_second_run_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path()).await;
        run_pass(&lib, 200, None).await.unwrap();
        let after_first = snapshot(&lib).await;
        // A small PNG with a thumbnail that appeared since: a recorded pass reads no headers.
        write_image(&dir.path().join("tex/s4.png"), 32, 32);
        write_image(&dir.path().join(".opendam/thumbs/s4_thumb.png"), 64, 64);
        add_asset(
            &lib.pool,
            "s4",
            "tex/s4.png",
            ".opendam/thumbs/s4_thumb.png",
        )
        .await;

        assert_eq!(run_pass(&lib, 200, None).await.unwrap(), Pass::AlreadyDone);

        let (rows, files, jobs) = snapshot(&lib).await;
        assert!(rows.contains(&row("s4", Some(".opendam/thumbs/s4_thumb.png"), "done")));
        assert!(files.contains(&"s4_thumb.png".to_string()));
        assert_eq!(jobs, after_first.2);
    }

    #[tokio::test]
    async fn an_interrupted_pass_redoes_its_batch_and_ends_like_an_uninterrupted_one() {
        let whole_dir = tempfile::tempdir().unwrap();
        let whole = library(whole_dir.path()).await;
        run_pass(&whole, 2, None).await.unwrap();

        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path()).await;
        // Batch 1 is s2 and s3; batch 0 (t1 and s1) was committed.
        assert_eq!(run_pass(&lib, 2, Some(1)).await.unwrap(), Pass::Interrupted);
        let (rows, files, jobs) = snapshot(&lib).await;
        assert!(rows.contains(&row("s1", None, "done")));
        // The batch's rows still point at their deleted files.
        assert!(rows.contains(&row("s2", Some(".opendam/thumbs/s2_thumb.png"), "done")));
        assert!(!files.contains(&"s2_thumb.png".to_string()));
        assert_eq!(jobs.len(), 1);
        assert_eq!(meta(&lib).await, None);

        // Interrupted in t1's own batch, before its job is committed.
        let first_dir = tempfile::tempdir().unwrap();
        let first = library(first_dir.path()).await;
        assert_eq!(
            run_pass(&first, 2, Some(0)).await.unwrap(),
            Pass::Interrupted
        );
        assert!(snapshot(&first).await.2.is_empty());
        assert_eq!(meta(&first).await, None);

        for lib in [&lib, &first] {
            assert_eq!(run_pass(lib, 2, None).await.unwrap(), Pass::Finished);
            assert_eq!(snapshot(lib).await, snapshot(&whole).await);
            assert_eq!(meta(lib).await.as_deref(), Some("done"));
        }
        assert_eq!(
            snapshot(&whole).await.2,
            vec![("t1".to_string(), "pending".to_string())]
        );
    }

    #[tokio::test]
    async fn a_rerun_after_the_native_size_thumbnail_is_written_queues_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let lib = library(dir.path()).await;
        run_pass(&lib, 200, None).await.unwrap();
        crate::preview::process_image_jobs(&lib, 2).await;
        assert_eq!(
            image::image_dimensions(dir.path().join(".opendam/thumbs/t1_thumb.png")).unwrap(),
            (64, 32)
        );
        // As if the pass had been interrupted after the job ran.
        sqlx::query("DELETE FROM library_meta")
            .execute(&lib.pool)
            .await
            .unwrap();

        assert_eq!(run_pass(&lib, 200, None).await.unwrap(), Pass::Finished);

        let (rows, _, jobs) = snapshot(&lib).await;
        assert!(rows.contains(&row("t1", Some(".opendam/thumbs/t1_thumb.png"), "done")));
        assert_eq!(jobs, vec![("t1".to_string(), "done".to_string())]);
    }
}
