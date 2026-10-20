use anyhow::Result;
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::db::{make_relative_path, Database, LibraryHandle};
use crate::models::AssetKind;
use crate::preview::is_native_image;

// ── Advisory lock guard ─────────────────────────────────────────────────────

/// RAII guard: writes `.opendam/index.lock` on creation, removes it on drop.
/// On NAS, multiple machines may attempt to re-index simultaneously on startup.
/// The second machine detects the lock and skips indexing, preventing interleaved
/// batch writes that would corrupt the asset catalogue.
struct IndexLock(PathBuf);

impl IndexLock {
    /// Try to acquire the lock.
    ///
    /// Returns `None` if another instance acquired the lock within the last
    /// `stale_after` seconds (default: 120s). Returns `Some(guard)` on success.
    fn try_acquire(opendam_dir: &Path, stale_after: u64) -> Option<Self> {
        let lock_path = opendam_dir.join("index.lock");

        // If the lock file exists and is recent, another indexer is running.
        if let Ok(meta) = std::fs::metadata(&lock_path) {
            if let Ok(modified) = meta.modified() {
                let age = modified
                    .duration_since(UNIX_EPOCH)
                    .map(|t| {
                        let now = std::time::SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        now.saturating_sub(t.as_secs())
                    })
                    .unwrap_or(u64::MAX);
                if age < stale_after {
                    tracing::info!(
                        "index.lock is {}s old — another machine is indexing, skipping.",
                        age
                    );
                    return None;
                }
            }
        }

        // Write current Unix timestamp into the lock file.
        let ts = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs()
            .to_string();

        if let Err(e) = std::fs::write(&lock_path, &ts) {
            // Couldn't write — NAS may be read-only. Log and proceed anyway
            // (best-effort: don't block indexing just because we can't lock).
            tracing::warn!(
                "Could not write index.lock at {:?}: {} — proceeding without lock",
                lock_path,
                e
            );
        }

        Some(IndexLock(lock_path))
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
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

/// Walk `paths` and index all found files as Assets into the database.
/// Highly optimized for NAS:
/// 1. Single initial query loads existing assets into RAM (0 queries during walk).
/// 2. Skips `.opendam` directory.
/// 3. Reuses each asset's existing thumbnail from the DB to never re-render previews across network.
/// 4. Groups writes into chunks of 200 inside single transactions (cutting NAS I/O by 99%).
/// 5. Advisory index.lock prevents two machines from indexing the same library simultaneously.
/// 6. Ends with the structure pass (`structure::rebuild`), under the same locks.
pub async fn index_paths(db: &Database, library_id: &str, paths: &[String]) -> Result<()> {
    // An offline library is skipped: walking a missing root finds nothing, and the
    // deleted-file pass below would then remove every asset row.
    let Some(LibraryHandle {
        pool, root_path, ..
    }) = db.get_library_handle(library_id).await
    else {
        tracing::info!("Library {} is not open, skipping indexing", library_id);
        return Ok(());
    };
    if !root_path.exists() {
        tracing::info!(
            "Library {} root {:?} is missing, skipping indexing",
            library_id,
            root_path
        );
        return Ok(());
    }

    // ── This app's lock: a command's structure pass waits for the run's own ──
    let index_lock = db.index_lock(library_id);
    let _index_guard = index_lock.lock().await;

    // ── Advisory lock: skip if another machine is already indexing ────────
    let opendam_dir = root_path.join(".opendam");
    let _lock = match IndexLock::try_acquire(&opendam_dir, 120) {
        Some(guard) => guard,
        None => return Ok(()), // another instance is indexing — yield
    };

    let now = Utc::now().timestamp_millis();

    // ── 1. Load existing assets in ONE single query into memory ──────────
    let existing_rows: Vec<(String, String, i64, i64, Option<String>)> =
        sqlx::query_as("SELECT id, file_path, modified_at, size_bytes, thumbnail_path FROM assets")
            .fetch_all(&pool)
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

            let rel_path = make_relative_path(&root_path, path);
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
                commit_asset_batch(&pool, &pending_batch).await?;
                pending_batch.clear();
            }
        }
    }

    // Flush any remaining assets
    if !pending_batch.is_empty() {
        commit_asset_batch(&pool, &pending_batch).await?;
        pending_batch.clear();
    }

    // ── 3. Detect and remove deleted files in batch ───────────────────────
    let to_delete: Vec<String> = existing_map
        .into_iter()
        .filter(|(rel, _)| !seen_paths.contains(rel))
        .map(|(_, (id, _, _, _))| id)
        .collect();

    if !to_delete.is_empty() {
        for chunk in to_delete.chunks(200) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("DELETE FROM assets WHERE id IN ({})", placeholders);
            let mut query = sqlx::query(&sql);
            for id in chunk {
                query = query.bind(id);
            }
            let _ = query.execute(&pool).await;
        }
    }

    // ── 4. Update asset count ───────────────────────────────────────────
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

    // ── 5. Derive the folder tree, still under both locks ─────────────────
    // A failed pass is logged, not returned: the run's writes went through, so the caller still
    // emits `library:updated`, and the next pass derives the folders again.
    if let Err(e) = crate::structure::rebuild(&pool, &root_path).await {
        tracing::error!(target: "structure", "Structure pass failed for library {library_id}: {e}");
    }

    tracing::info!(
        "NAS-efficient indexing complete for library {}: {} total assets",
        library_id,
        total_count
    );
    Ok(())
}

/// Helper to execute a batch of assets inside a single database transaction
async fn commit_asset_batch(pool: &sqlx::Pool<sqlx::Sqlite>, batch: &[PendingAsset]) -> Result<()> {
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
}
