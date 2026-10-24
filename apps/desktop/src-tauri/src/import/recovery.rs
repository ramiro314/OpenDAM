//! Swap recovery (plan pack-import, Flow, "Swap recovery"). A build sets and clears a pack's
//! `swap_state` only while it holds the library's `index_lock` and advisory lock, so a caller
//! holding both that finds a pack `swapping` knows its build is gone, on this machine or another.
//! Recovery reads what is on disk beside the pack and puts the pack back as the committed build
//! left it. It never rolls a build forward: an uncommitted build is always discarded.
//!
//! Two tables, each with its own trigger:
//!
//! - **`swapping`**, at a library open and at the start of every locking index or structure entry
//!   point (`watcher::index_paths`, `structure::rebuild_library`).
//! - **Leftovers** of packs whose `swap_state` is `NULL` (staging, `.failed-*` and `.prev-*`
//!   folders), only at a library open, skipping every pack with a live run (`live_run`), so no
//!   refresh deletes the staging folder of a build in progress.
//!
//! An index it runs is a lock-free core (`watcher::index_pool`), since at a library open the
//! library isn't registered yet, and only over a pack folder that exists, so a missing folder is
//! never pruned. The structure pass is its caller's, which runs one after it.

use std::path::{Path, PathBuf};

use sqlx::{Pool, Sqlite};

use super::build::{self, index_pack, Hooks, Refresh, Rollback, FAILED, PREV, STAGING};
use super::engine::{now_ms, owner_gone};
use super::records::{self, Owner};
use crate::fs_swap;
use crate::watcher::{self, IndexLock};

/// Which swap recovery tables a caller runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// A library opening: packs left `swapping`, then the leftovers of packs with no live run.
    Open,
    /// The start of a locking index or structure entry point: packs left `swapping` only.
    Entry,
}

/// What a run marked `interrupted` by recovery says, as the engine's startup does.
const INTERRUPTED: &str = "OpenDAM stopped during the run";

/// Who judges whether a run is live, and when: this process and the clock, outside tests.
pub struct Liveness {
    pub me: Owner,
    /// Milliseconds since the epoch.
    pub now: i64,
}

impl Liveness {
    pub fn now() -> Self {
        Liveness {
            me: watcher::this_process(),
            now: now_ms(),
        }
    }
}

/// Swap recovery for the library DB at `pool`, whose files lie below `root`. Its callers hold
/// the library's `index_lock` and advisory lock, passed as proof (`index_guard`, `advisory`),
/// and call it before they touch a row: the library open (`db::open_library_db`),
/// `watcher::index_paths` and `structure::rebuild_library`, its follow-up task included. A pack
/// it can't recover keeps its journal, for the next caller, and the failure is logged.
pub async fn recover_swaps(
    pool: &Pool<Sqlite>,
    root: &Path,
    tables: Recovery,
    index_guard: &tokio::sync::MutexGuard<'_, ()>,
    advisory: &IndexLock,
) {
    recover_swaps_with(pool, root, tables, index_guard, advisory, &Liveness::now()).await
}

/// `recover_swaps`, judging the leftovers table's live runs by `liveness`.
pub(super) async fn recover_swaps_with(
    pool: &Pool<Sqlite>,
    root: &Path,
    tables: Recovery,
    index_guard: &tokio::sync::MutexGuard<'_, ()>,
    advisory: &IndexLock,
    liveness: &Liveness,
) {
    let journaled: Vec<Journal> = match sqlx::query_as(
        "SELECT id, target, swap_state, swap_build, swap_prev FROM import_packs
         WHERE swap_state IS NOT NULL ORDER BY target",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("Could not read the swap journals of the library at {root:?}: {e}");
            return;
        }
    };
    for pack in &journaled {
        if pack.swap_state != "swapping" {
            // Remove import isn't built yet, so nothing journals `removing`: its recovery comes
            // with it. The pack stays as found, its runs refused.
            tracing::error!(
                "Pack {} is journaled {:?}, which this version doesn't recover",
                pack.target,
                pack.swap_state
            );
            continue;
        }
        if let Err(e) = recover_swap(pool, root, index_guard, advisory, pack).await {
            tracing::error!(
                "Recovering the swap of pack {}: {e}. Its journal stays for the next try",
                pack.target
            );
        }
    }
    if tables == Recovery::Open {
        if let Err(e) = leftovers(pool, root, advisory, liveness).await {
            tracing::error!("Clearing the build leftovers of the library at {root:?}: {e}");
        }
    }
}

/// A pack whose `swap_state` is set.
#[derive(sqlx::FromRow)]
struct Journal {
    id: String,
    target: String,
    swap_state: String,
    swap_build: Option<String>,
    swap_prev: Option<bool>,
}

/// What a `swapping` pack's folders tell, by the rows of the plan's first table.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    /// Crashed after the journal and before the swap, or after a failed first rename.
    Staged,
    /// Crashed after the first rename, the staging folder still there or not.
    Aside { staging: bool },
    /// Swapped in and not committed, with a previous folder aside or none.
    Swapped { prev: bool },
    /// Crashed in a rollback, between its renames (`prev`) or after them.
    RollingBack { prev: bool },
    /// Nothing of the build is on disk: step 4 failed and the previous folder (if any) was put
    /// back, or the staging folder was gone before the swap.
    Nothing,
}

fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// Recovers one `swapping` pack, by the plan's first table.
async fn recover_swap(
    pool: &Pool<Sqlite>,
    root: &Path,
    index_guard: &tokio::sync::MutexGuard<'_, ()>,
    advisory: &IndexLock,
    pack: &Journal,
) -> Result<(), String> {
    let live = root.join(&pack.target);
    let (Some(build_id), Some(swap_prev)) = (&pack.swap_build, pack.swap_prev) else {
        return unrecognised(pool, pack, "its journal names no build").await;
    };
    if !build::plainly_inside(root, &live) {
        return unrecognised(pool, pack, "a folder on the way to it is a link").await;
    }
    let staging = fs_swap::sibling(&live, STAGING, build_id);
    let prev = fs_swap::sibling(&live, PREV, build_id);
    let failed = fs_swap::sibling(&live, FAILED, build_id);
    let on_disk = (
        exists(&staging),
        exists(&prev),
        exists(&live),
        exists(&failed),
    );
    let found = match on_disk {
        (true, false, _, false) => Found::Staged,
        (true, true, false, false) => Found::Aside { staging: true },
        (false, true, false, false) => Found::Aside { staging: false },
        (false, true, true, false) => Found::Swapped { prev: true },
        (false, false, true, false) if !swap_prev => Found::Swapped { prev: false },
        (false, true, false, true) => Found::RollingBack { prev: true },
        (false, false, _, true) => Found::RollingBack { prev: false },
        (false, false, true, false) => Found::Nothing,
        (false, false, false, false) if !swap_prev => Found::Nothing,
        (s, p, l, f) => {
            let state = format!(
                "staging {}, previous folder {}, pack folder {}, failed folder {}, \
                 a live folder moved aside: {swap_prev}",
                present(s),
                present(p),
                present(l),
                present(f)
            );
            return unrecognised(pool, pack, &state).await;
        }
    };
    tracing::warn!(
        "Pack {} was left mid-swap by build {build_id} ({found:?}): recovering it",
        pack.target
    );
    let rename = |from: &Path, to: &Path| {
        std::fs::rename(from, to).map_err(|e| {
            format!(
                "could not rename {} to {}: {e}",
                from.display(),
                to.display()
            )
        })
    };
    match found {
        Found::Staged => {
            fs_swap::discard(&staging);
            unjournal(pool, &pack.id, build_id).await?;
        }
        Found::Aside { staging: present } => {
            rename(&prev, &live)?;
            if present {
                fs_swap::discard(&staging);
            }
            unjournal(pool, &pack.id, build_id).await?;
        }
        Found::Swapped { prev: aside } => {
            rename(&live, &failed)?;
            if aside {
                rename(&prev, &live)?;
            }
            finish(
                pool,
                root,
                index_guard,
                advisory,
                pack,
                build_id,
                swap_prev,
                &failed,
            )
            .await?;
        }
        Found::RollingBack { prev: aside } => {
            if aside {
                rename(&prev, &live)?;
            }
            finish(
                pool,
                root,
                index_guard,
                advisory,
                pack,
                build_id,
                swap_prev,
                &failed,
            )
            .await?;
        }
        Found::Nothing => {
            unjournal(pool, &pack.id, build_id).await?;
            if swap_prev {
                index_pack(pool, root, advisory, &live).await;
            }
        }
    }
    interrupt_runs(pool, &pack.id).await;
    Ok(())
}

fn present(found: bool) -> &'static str {
    if found {
        "present"
    } else {
        "absent"
    }
}

/// A rollback's steps 3 and 4 once its folders are in place, as a build's own rollback runs them
/// (`build::finish_rollback`), through the lock-free core: the library may not be registered yet,
/// and the structure pass is the caller's. The pack is reindexed only when a live folder was moved
/// aside (`swap_prev`).
#[allow(clippy::too_many_arguments)]
async fn finish(
    pool: &Pool<Sqlite>,
    root: &Path,
    index_guard: &tokio::sync::MutexGuard<'_, ()>,
    advisory: &IndexLock,
    pack: &Journal,
    build_id: &str,
    swap_prev: bool,
    failed: &Path,
) -> Result<(), String> {
    let live = root.join(&pack.target);
    let rolled = Rollback {
        pool,
        pack_id: &pack.id,
        target: &pack.target,
        build_id,
        live: &live,
        failed,
        reindex: swap_prev,
    };
    let refresh = Refresh::Core { root };
    build::finish_rollback(&rolled, refresh, index_guard, advisory, &Hooks::default())
        .await
        .map_err(|step| format!("finishing the rollback failed ({step})"))
}

async fn unjournal(pool: &Pool<Sqlite>, pack_id: &str, build_id: &str) -> Result<(), String> {
    build::unjournal(pool, pack_id, build_id)
        .await
        .map_err(|e| format!("could not clear the journal: {e}"))
}

/// Marks pack `pack_id`'s `running` runs `interrupted`: their build is gone.
async fn interrupt_runs(pool: &Pool<Sqlite>, pack_id: &str) {
    if let Err(e) = end_runs(pool, pack_id, "interrupted", INTERRUPTED).await {
        tracing::error!("Recovered pack {pack_id}, but {e}");
    }
}

/// Ends pack `pack_id`'s `running` runs as `status`, with `error`.
async fn end_runs(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    status: &str,
    error: &str,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE import_runs SET status = ?, error = ?, finished_at = ?
         WHERE status = 'running' AND EXISTS (SELECT 1 FROM json_each(import_runs.packs) j
             WHERE json_extract(j.value, '$.packId') = ?)",
    )
    .bind(status)
    .bind(error)
    .bind(now_ms())
    .bind(pack_id)
    .execute(pool)
    .await
    .map(|_| ())
    .map_err(|e| format!("could not end the pack's runs: {e}"))
}

/// The last row of the first table: a state no step leaves. Changes nothing on disk and leaves
/// `swap_state` set, which refuses the pack's runs and step edits, marks the pack `failed`, and
/// ends its running runs naming the state found.
async fn unrecognised(pool: &Pool<Sqlite>, pack: &Journal, state: &str) -> Result<(), String> {
    let message = format!(
        "The pack {} was left mid-swap in a state no step leaves ({state}). It is left as found",
        pack.target
    );
    tracing::error!("{message}");
    sqlx::query("UPDATE import_packs SET status = 'failed' WHERE id = ?")
        .bind(&pack.id)
        .execute(pool)
        .await
        .map_err(|e| format!("could not mark the pack failed: {e}"))?;
    end_runs(pool, &pack.id, "failed", &message).await
}

/// What a folder left beside a pack whose `swap_state` is `NULL` is, by its `fs_swap` tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Left {
    /// `.import-*`: a build crashed while staging.
    Staging,
    /// `.failed-*`: a rollback finished its transaction, then crashed before deleting it.
    Failed,
    /// `.prev-*`: a build committed, then crashed before it settled.
    Prev,
}

impl Left {
    /// In the order they're cleared: the staging and failed folders before a settle's index.
    const ALL: [Left; 3] = [Left::Staging, Left::Failed, Left::Prev];

    fn tag(self) -> &'static str {
        match self {
            Left::Staging => STAGING,
            Left::Failed => FAILED,
            Left::Prev => PREV,
        }
    }
}

/// A folder left beside a pack whose `swap_state` is `NULL`, with its build id.
struct Leftover {
    kind: Left,
    folder: PathBuf,
    build_id: String,
}

/// The second table, for every pack whose `swap_state` is `NULL`: deletes a staging folder and
/// its build's rows, deletes a `.failed-*` folder, and settles a `.prev-*` one (step 7). A pack
/// with a live run is skipped. Its folders are listed before its runs are read, so a build that
/// starts meanwhile never has its staging folder deleted, nor its rows (`settle_records`).
async fn leftovers(
    pool: &Pool<Sqlite>,
    root: &Path,
    advisory: &IndexLock,
    liveness: &Liveness,
) -> sqlx::Result<()> {
    let packs: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, target, current_build FROM import_packs WHERE swap_state IS NULL
         ORDER BY target",
    )
    .fetch_all(pool)
    .await?;
    for (pack_id, target, current_build) in packs {
        let live = root.join(&target);
        if !build::plainly_inside(root, &live) {
            continue;
        }
        let found = left_beside(&live);
        if found.is_empty() {
            continue;
        }
        if live_run(pool, &pack_id, liveness).await? {
            tracing::info!("Pack {target} has a live run, so its build folders stay");
            continue;
        }
        for left in found {
            tracing::warn!("Clearing {} beside pack {target}", left.folder.display());
            match left.kind {
                Left::Staging => {
                    fs_swap::discard(&left.folder);
                    if current_build.as_deref() != Some(left.build_id.as_str()) {
                        let mut tx = pool.begin().await?;
                        records::delete_build(&mut tx, &pack_id, &left.build_id).await?;
                        tx.commit().await?;
                    }
                }
                Left::Failed => fs_swap::discard(&left.folder),
                Left::Prev => {
                    index_pack(pool, root, advisory, &live).await;
                    fs_swap::discard(&left.folder);
                    settle_records(pool, &pack_id, current_build.as_deref(), &live).await?;
                }
            }
        }
    }
    Ok(())
}

/// A `.prev-*` settle's records: deletes the manifest rows of every build of pack `pack_id` but
/// `current` and those still staging, and the pack's journal. A build that started staging after
/// the live-run check (staging takes no lock) keeps its rows: the transaction's first write takes
/// the database's write lock, and only then are the staging folders beside `live` listed. A build
/// creates its folder before it writes its rows, so a build whose rows were written by then has
/// its folder listed, and one that writes them later waits for this commit. The journal is written
/// only under the locks this recovery holds, so no other build has one.
async fn settle_records(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    current: Option<&str>,
    live: &Path,
) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM import_moves WHERE pack_id = ?")
        .bind(pack_id)
        .execute(&mut *tx)
        .await?;
    let staging: Vec<String> = left_beside(live)
        .into_iter()
        .filter(|left| left.kind == Left::Staging)
        .map(|left| left.build_id)
        .collect();
    let builds: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT build_id FROM import_outputs WHERE pack_id = ?")
            .bind(pack_id)
            .fetch_all(&mut *tx)
            .await?;
    for build in builds {
        if Some(build.as_str()) == current || staging.contains(&build) {
            continue;
        }
        sqlx::query("DELETE FROM import_outputs WHERE pack_id = ? AND build_id = ?")
            .bind(pack_id)
            .bind(&build)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

/// The staging, failed and previous folders beside the pack folder `live`, in that order. Only a
/// name ending in a build id (a UUID) counts, so another pack's folder whose name starts the same
/// way is never taken for one.
fn left_beside(live: &Path) -> Vec<Leftover> {
    let mut found = Vec::new();
    for kind in Left::ALL {
        let tag = kind.tag();
        let folders = match fs_swap::leftovers(live, tag) {
            Ok(folders) => folders,
            Err(e) => {
                tracing::warn!("Could not list the folders beside {}: {e}", live.display());
                continue;
            }
        };
        let prefix = fs_swap::prefix(live, tag);
        for folder in folders {
            let name = folder.file_name().unwrap_or_default().to_string_lossy();
            let build_id = name[prefix.len()..].to_string();
            if uuid::Uuid::parse_str(&build_id).is_ok() {
                found.push(Leftover {
                    kind,
                    folder,
                    build_id,
                });
            }
        }
    }
    found
}

/// Whether pack `pack_id` has a live run: one `queued` or `running` whose owner is alive, judged
/// by its process on this machine and by its heartbeat on another (plan Flow, Live runs), not by
/// its status alone, as `liveness` sees it.
async fn live_run(pool: &Pool<Sqlite>, pack_id: &str, liveness: &Liveness) -> sqlx::Result<bool> {
    #[allow(clippy::type_complexity)]
    let runs: Vec<(Option<String>, Option<String>, i64, i64, Option<i64>)> = sqlx::query_as(
        "SELECT owner_machine, owner_boot, owner_pid, owner_started, heartbeat_at
         FROM import_runs WHERE status IN ('queued', 'running')
         AND EXISTS (SELECT 1 FROM json_each(import_runs.packs) j
             WHERE json_extract(j.value, '$.packId') = ?)",
    )
    .bind(pack_id)
    .fetch_all(pool)
    .await?;
    let Liveness { me, now } = liveness;
    Ok(runs.iter().any(|(machine, boot, pid, started, heartbeat)| {
        !owner_gone(me, machine, boot, *pid, *started, *heartbeat, *now)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn insert_output(pool: &Pool<Sqlite>, build_id: &str) {
        sqlx::query(
            "INSERT INTO import_outputs (pack_id, build_id, path, size, mtime_ns, sha256, origin,
             source_path) VALUES ('p', ?, 'Pub/Pack/a.png', 1, 0, '', 'extracted', '[\"a\"]')",
        )
        .bind(build_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_prev_settle_keeps_the_rows_of_a_build_that_started_staging_meanwhile() {
        let pool = crate::db::memory_library_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("Pub/Pack");
        std::fs::create_dir_all(&live).unwrap();
        let (current, old, staging) = (
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
            uuid::Uuid::new_v4().to_string(),
        );
        // A rebuild that began staging after the live-run check: its folder, then its rows.
        fs_swap::create_staging(&live, STAGING, &staging).unwrap();
        for build in [&current, &old, &staging] {
            insert_output(&pool, build).await;
        }
        sqlx::query(
            "INSERT INTO import_moves (pack_id, build_id, seq, kind, asset_id, to_path)
             VALUES ('p', ?, 1, 'inserted', 'x', 'Pub/Pack/a.png')",
        )
        .bind(&current)
        .execute(&pool)
        .await
        .unwrap();

        settle_records(&pool, "p", Some(&current), &live)
            .await
            .unwrap();

        let mut left: Vec<String> =
            sqlx::query_scalar("SELECT build_id FROM import_outputs WHERE pack_id = 'p'")
                .fetch_all(&pool)
                .await
                .unwrap();
        left.sort();
        let mut kept = vec![current, staging];
        kept.sort();
        assert_eq!(left, kept, "only the old build's rows go");
        let moves: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM import_moves")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(moves, 0);
    }

    #[tokio::test]
    async fn another_machine_s_staging_stays_while_its_heartbeat_is_fresh_by_the_test_clock() {
        let pool = crate::db::memory_library_pool().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".opendam")).unwrap();
        let live = dir.path().join("Pub/Pack");
        let build = uuid::Uuid::new_v4().to_string();
        let staging = fs_swap::create_staging(&live, STAGING, &build).unwrap();
        insert_output(&pool, &build).await;
        let beat = 1_000_000_000_000_i64;
        for sql in [
            "INSERT INTO imports (id, original_path, original_size, original_mtime_ns,
             original_sha256, kind, created_at) VALUES ('i', '/o.unitypackage', 1, 1, '',
             'unitypackage', 0)",
            "INSERT INTO import_packs (id, import_id, target, preset) VALUES ('p', 'i', 'Pub/Pack',
             'unity')",
            "INSERT INTO import_runs (id, kind, packs, status, created_at, owner_machine,
             owner_boot, owner_pid, owner_started, heartbeat_at) VALUES ('r', 'import',
             '[{\"packId\":\"p\",\"recipeRev\":0}]', 'running', 0, 'another machine', 'b',
             1, 1, ?1)",
        ] {
            sqlx::query(sql).bind(beat).execute(&pool).await.unwrap();
        }
        let index_lock = tokio::sync::Mutex::new(());
        let recover = |minutes: i64| {
            let (pool, root, index_lock) = (&pool, dir.path(), &index_lock);
            async move {
                let index_guard = index_lock.lock().await;
                let advisory = IndexLock::try_acquire(root).await.unwrap();
                let liveness = Liveness {
                    me: watcher::this_process(),
                    now: beat + minutes * 60 * 1000,
                };
                recover_swaps_with(
                    pool,
                    root,
                    Recovery::Open,
                    &index_guard,
                    &advisory,
                    &liveness,
                )
                .await;
            }
        };
        let rows = || async {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM import_outputs")
                .fetch_one(&pool)
                .await
                .unwrap()
        };

        recover(1).await;
        assert!(staging.exists(), "a heartbeat a minute old is a live run's");
        assert_eq!(rows().await, 1);

        recover(11).await;
        assert!(!staging.exists(), "one 11 minutes old is a dead run's");
        assert_eq!(rows().await, 0);
    }
}
