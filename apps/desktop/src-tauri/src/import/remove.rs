//! Remove import (plan pack-import, Remove import, R38, A13): a `remove` run deletes exactly the
//! files a pack's current build wrote (kept files included) and the folders they leave empty,
//! prunes their rows, then drops the pack's records, journaled like a swap:
//!
//! 1. **Lock.** Takes the library's `index_lock`, then the advisory lock, and holds both to the
//!    end, as a build does. Cancel takes effect only before this point.
//! 2. **Re-check.** Stops, with nothing changed, unless the recipe is as queued and the pack
//!    folder still differs from the current manifest exactly as the confirmation showed it.
//! 3. **Journal.** Sets `swap_state = 'removing'`, which refuses the pack's runs and edits.
//! 4. **Delete files.** Deletes the current manifest's files, a file already gone being no
//!    error, the folders left empty, and any staging, previous or failed folder of the pack's
//!    builds beside it.
//! 5. **Prune.** Indexes the pack folder with `Prune::Now`, which deletes the removed files' rows
//!    (and their tags and collections) and only rows at or under the folder.
//! 6. **Records.** Deletes the pack's records, and its import's row once no pack is left
//!    (`records::delete_pack`).
//!
//! Steps 4-6 can be repeated, so recovery finishes a pack it finds `removing` from step 4, with
//! no re-check: the folder no longer matches the manifest by then (`recovery::recover_swaps`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sqlx::{Pool, Sqlite};

use super::build::{self, plainly_inside, BuildEnv, Hooks, Point, Progress, Refresh, Stop};
use super::drift::{self, Seen};
use super::records::{self, ManifestRow};
use super::recovery;
use crate::fs_swap;
use crate::watcher::{self, IndexLock, Prune};

const CHANGED: &str = "The pack changed since its Remove was asked for; Remove it again";

/// How many files step 4 deletes between its checks, few in tests so a crash lands midway.
const DELETE_BATCH: usize = if cfg!(test) { 4 } else { 200 };

/// A file of the pack as the Remove's confirmation lists it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovedFile {
    /// Relative to the library root.
    pub path: String,
    /// `extracted`, `generated` or `kept`.
    pub origin: &'static str,
}

/// What a Remove of a pack would delete, and its outside changes, for the confirmation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovePreview {
    /// The current manifest's files, by path.
    pub files: Vec<RemovedFile>,
    /// How the pack folder differs from them now: a changed file's bytes are lost, an added
    /// file stays.
    pub drift: Vec<drift::Difference>,
}

/// Pack `pack_id`'s Remove preview, its manifest and drift read at the same `recipe_rev`, which
/// it returns with them for the Remove to be queued at.
pub async fn preview(
    pool: &Pool<Sqlite>,
    root: &Path,
    pack_id: &str,
) -> Result<(RemovePreview, i64), String> {
    let recipe = records::recipe(pool, pack_id)
        .await
        .map_err(|e| format!("Database error: {e}"))?;
    let manifest = current_manifest(pool, pack_id, recipe.current_build.as_deref())
        .await
        .map_err(|e| format!("Database error: {e}"))?;
    let drift = drift::check_recipe(pool, root, &recipe).await?;
    let files = manifest
        .iter()
        .map(|row| RemovedFile {
            path: row.path.clone(),
            origin: row.origin.as_str(),
        })
        .collect();
    Ok((RemovePreview { files, drift }, recipe.recipe_rev))
}

async fn current_manifest(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    build: Option<&str>,
) -> sqlx::Result<Vec<ManifestRow>> {
    match build {
        Some(build) => records::manifest(pool, pack_id, build).await,
        None => Ok(Vec::new()),
    }
}

/// Removes pack `pack_id`, queued at `queued_rev` with `seen`, the outside changes its
/// confirmation showed.
pub async fn remove(
    env: &BuildEnv<'_>,
    pack_id: &str,
    queued_rev: i64,
    seen: &[Seen],
) -> Result<(), Stop> {
    let pool = &env.library.pool;
    let root = env.library.root_path.clone();
    let queued = records::recipe(pool, pack_id)
        .await
        .map_err(|e| Stop::Failed(format!("Database error: {e}")))?;
    if queued.swap_state.is_some() || queued.recipe_rev != queued_rev {
        return Err(Stop::Failed(CHANGED.into()));
    }

    // ── 1. Lock ─────────────────────────────────────────────────────────
    (env.progress)(Progress {
        step: "waiting for the library",
        done: 0,
        total: 0,
    });
    let index_lock = env.db.index_lock(&env.library.id);
    let (index_guard, advisory) = build::both_locks(env, &index_lock, &root).await?;
    let removed = if env.control.lock() {
        locked(env, &queued, seen, &index_guard, &advisory).await
    } else {
        Err(Stop::Cancelled)
    };
    advisory.release(pool, &root).await;
    drop(index_guard);
    removed
}

/// Steps 2-6, under both locks, with Cancel past.
async fn locked(
    env: &BuildEnv<'_>,
    queued: &records::Recipe,
    seen: &[Seen],
    index_guard: &tokio::sync::MutexGuard<'_, ()>,
    advisory: &IndexLock,
) -> Result<(), Stop> {
    let pool = &env.library.pool;
    let root = &env.library.root_path;
    (env.progress)(Progress {
        step: "re-checking",
        done: 0,
        total: 0,
    });

    // ── 2. Re-check ─────────────────────────────────────────────────────
    let now = records::recipe(pool, &queued.pack_id)
        .await
        .map_err(|e| Stop::Failed(format!("Database error: {e}")))?;
    let same = now.recipe_rev == queued.recipe_rev
        && now.current_build == queued.current_build
        && now.swap_state.is_none()
        && now.target == queued.target;
    if !same || !plainly_inside(root, &root.join(&now.target)) {
        return Err(Stop::Failed(CHANGED.into()));
    }
    let found = drift::check_recipe(pool, root, &now).await?;
    if let Some(path) = drift::unseen(&found, seen) {
        return Err(Stop::Failed(format!(
            "{path} changed outside OpenDAM since the Remove was asked for: nothing was \
             deleted. Remove it again"
        )));
    }
    if !advisory.still_held() {
        return Err(Stop::Failed(
            "Another machine took the library's index lock: nothing was deleted. Remove it again"
                .into(),
        ));
    }

    // ── 3. Journal ──────────────────────────────────────────────────────
    let journaled = sqlx::query(
        "UPDATE import_packs SET swap_state = 'removing', swap_build = NULL, swap_prev = NULL
         WHERE id = ? AND swap_state IS NULL",
    )
    .bind(&queued.pack_id)
    .execute(pool)
    .await;
    if !matches!(&journaled, Ok(done) if done.rows_affected() == 1) {
        return Err(Stop::Failed(CHANGED.into()));
    }
    env.hooks.stop(Point::RemoveJournaled).await;

    // ── 4-6. Delete files, prune, records ───────────────────────────────
    (env.progress)(Progress {
        step: "removing",
        done: 0,
        total: 0,
    });
    let pack = Removing {
        pool,
        root,
        pack_id: &queued.pack_id,
        target: &queued.target,
    };
    let refresh = Refresh::Registered {
        db: env.db,
        library: env.library,
    };
    pack.finish(refresh, index_guard, advisory, env.hooks)
        .await
        .map_err(|e| {
            Stop::Failed(format!(
                "Removing the pack failed ({e}), so it is finished when the library next opens \
                 or is refreshed"
            ))
        })
}

/// A pack journaled `removing`, in the library DB at `pool` whose files lie below `root`.
pub(super) struct Removing<'a> {
    pub pool: &'a Pool<Sqlite>,
    pub root: &'a Path,
    pub pack_id: &'a str,
    pub target: &'a str,
}

impl Removing<'_> {
    /// Steps 4-6, which a Remove runs and recovery repeats, both holding the library's locks. A
    /// step that fails leaves the journal and the records, for the next try.
    pub(super) async fn finish(
        &self,
        refresh: Refresh<'_>,
        index_guard: &tokio::sync::MutexGuard<'_, ()>,
        advisory: &IndexLock,
        hooks: &Hooks,
    ) -> Result<(), String> {
        let live = self.root.join(self.target);
        // An unmounted library would look like a pack whose files are all gone already.
        if !self.root.is_dir() {
            return Err("the library's folder can't be found".into());
        }
        if !plainly_inside(self.root, &live) {
            return Err(format!("a folder on the way to {} is a link", self.target));
        }
        let build: Option<String> =
            sqlx::query_scalar("SELECT current_build FROM import_packs WHERE id = ?")
                .bind(self.pack_id)
                .fetch_one(self.pool)
                .await
                .map_err(|e| format!("reading the pack: {e}"))?;
        let manifest = current_manifest(self.pool, self.pack_id, build.as_deref())
            .await
            .map_err(|e| format!("reading its manifest: {e}"))?;

        // ── 4. Delete files ─────────────────────────────────────────────
        let paths: Vec<PathBuf> = manifest
            .iter()
            .map(|row| self.root.join(&row.path))
            .collect();
        for batch in paths.chunks(DELETE_BATCH) {
            let (batch, live) = (batch.to_vec(), live.clone());
            tokio::task::spawn_blocking(move || delete_files(&live, &batch))
                .await
                .map_err(|e| format!("the delete stopped: {e}"))??;
            hooks.crashes(Point::RemoveDeleting);
        }
        let live_dir = live.clone();
        tokio::task::spawn_blocking(move || {
            remove_empty_folders(&live_dir, &paths);
            for left in recovery::left_beside(&live_dir) {
                fs_swap::discard(&left.folder);
            }
        })
        .await
        .map_err(|e| format!("the delete stopped: {e}"))?;

        // ── 5. Prune ────────────────────────────────────────────────────
        // A pack folder that is gone is walked too: its rows are exactly the ones to prune.
        let pack_path = [live.to_string_lossy().into_owned()];
        let pruned = match refresh {
            Refresh::Registered { db, library } => {
                watcher::index_paths_locked(
                    db,
                    &library.id,
                    &pack_path,
                    Prune::Now,
                    index_guard,
                    advisory,
                )
                .await
            }
            Refresh::Core { root } => {
                watcher::index_pool(self.pool, root, &pack_path, Prune::Now, Some(advisory)).await
            }
        };
        if let Err(e) = pruned {
            // The records still go: the next refresh prunes the rows of files that are gone.
            tracing::warn!("Pruning the removed pack {}: {e:#}", self.target);
        }
        hooks.crashes(Point::RemovePruned);

        // ── 6. Records ──────────────────────────────────────────────────
        records::delete_pack(self.pool, self.pack_id)
            .await
            .map_err(|e| format!("deleting its records: {e}"))?;
        tracing::info!("Removed the imported pack {}", self.target);
        Ok(())
    }
}

/// Deletes each regular file of `paths` in the pack folder `live`. A file already gone is no
/// error. One whose folder on the way is a link, or that is no regular file (a link, a folder),
/// is left alone, so a Remove never deletes anything outside the pack.
fn delete_files(live: &Path, paths: &[PathBuf]) -> Result<(), String> {
    for path in paths {
        let meta = match path.symlink_metadata() {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        let inside = path.parent().is_some_and(|dir| plainly_inside(live, dir));
        if !meta.is_file() || !inside {
            tracing::warn!(
                "{} is no regular file of the pack now, so the Remove leaves it",
                path.display()
            );
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("deleting {}: {e}", path.display())),
        }
    }
    Ok(())
}

/// Deletes the folders between the files `paths` and the pack folder `live`, `live` included,
/// that are empty now, deepest first. A folder holding anything else stays.
fn remove_empty_folders(live: &Path, paths: &[PathBuf]) {
    let mut folders: BTreeSet<(usize, PathBuf)> = BTreeSet::new();
    for path in paths {
        for dir in path.ancestors().skip(1) {
            if !dir.starts_with(live) {
                break;
            }
            folders.insert((dir.components().count(), dir.to_path_buf()));
        }
    }
    for (_, dir) in folders.into_iter().rev() {
        // Fails, as it should, while anything is in it, or once it's gone
        let _ = std::fs::remove_dir(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_folders_left_empty_go_deepest_first_and_never_above_the_pack() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("Pub/Pack");
        for folder in ["a/b", "c", "d"] {
            std::fs::create_dir_all(live.join(folder)).unwrap();
        }
        std::fs::write(live.join("c/yours.txt"), "kept").unwrap();
        let paths = vec![live.join("a/b/x.png"), live.join("c/y.png")];
        remove_empty_folders(&live, &paths);
        assert!(
            !live.join("a").exists(),
            "a and a/b held only the pack's files"
        );
        assert!(live.join("c/yours.txt").exists());
        assert!(
            live.join("d").exists(),
            "a folder no pack file was in stays"
        );
        assert!(dir.path().join("Pub").exists());

        std::fs::remove_dir_all(live.join("d")).unwrap();
        std::fs::remove_file(live.join("c/yours.txt")).unwrap();
        remove_empty_folders(&live, &paths);
        assert!(!live.exists(), "the pack folder goes once empty");
        assert!(dir.path().join("Pub").exists(), "never a folder above it");
    }

    #[test]
    fn a_file_reached_through_a_link_or_that_is_a_link_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (live, outside) = (dir.path().join("Pack"), dir.path().join("Outside"));
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("a.png"), "a").unwrap();
        std::fs::write(live.join("own.png"), "b").unwrap();
        std::os::unix::fs::symlink(&outside, live.join("Linked")).unwrap();
        std::os::unix::fs::symlink(outside.join("a.png"), live.join("link.png")).unwrap();
        let paths = [
            live.join("Linked/a.png"),
            live.join("link.png"),
            live.join("own.png"),
            live.join("gone.png"),
        ];
        delete_files(&live, &paths).unwrap();
        assert!(outside.join("a.png").exists());
        assert!(live.join("link.png").symlink_metadata().is_ok());
        assert!(!live.join("own.png").exists());
    }

    #[test]
    fn a_remove_sees_exactly_the_drift_its_confirmation_showed() {
        let difference = |path: &str, change, sha: &str| drift::Difference {
            path: path.into(),
            change,
            kind: drift::Kind::Extracted,
            size: Some(1),
            sha256: Some(sha.into()),
            choices: vec![],
            reason: None,
            source_path: None,
        };
        let found = [
            difference("P/a.png", drift::Change::Changed, "1"),
            difference("P/b.png", drift::Change::Added, "2"),
        ];
        let seen: Vec<Seen> = found.iter().map(Seen::from).collect();
        assert_eq!(drift::unseen(&found, &seen), None);
        assert_eq!(drift::unseen(&found, &seen[..1]), Some("P/b.png".into()));
        assert_eq!(drift::unseen(&found[..1], &seen), Some("P/b.png".into()));
        let mut other = seen.clone();
        other[0].sha256 = Some("3".into());
        assert_eq!(drift::unseen(&found, &other), Some("P/a.png".into()));
        assert_eq!(drift::unseen(&[], &[]), None);
    }
}
