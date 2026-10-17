//! Undo of a run (docs/plans/ai-auto-tagging/plan.md, "Undo (D23) and offline libraries"):
//! it removes exactly the pairs the run recorded in `tag_run_applied`, then deletes the tags it
//! created that nothing uses any more. A library that is offline keeps its rows, and the run
//! stays `undo_partial` until Finish undo runs the same steps with the library back.

use sqlx::{Pool, Sqlite};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;

use super::store::BATCH;
use crate::commands::tag::delete_tag_in;
use crate::db::{Database, LibraryHandle};

/// Test-only crash points: an operation reaching the one set stops there for good, as if the
/// app died, so a test can restart on the same databases. Outside tests nothing ever stops.
#[derive(Default)]
pub struct Crashes {
    #[cfg(test)]
    at: std::sync::Mutex<Option<CrashPoint>>,
    #[cfg(test)]
    hit: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// An item committed step 1 of the write protocol.
    AfterIntent,
    /// A Create freely item inserted a new tag into master, inside its step 1 transaction.
    AfterTagInsert,
    /// An item committed step 3, its library write.
    AfterLibraryCommit,
    /// Undo deleted a batch from a library, not yet the matching master rows.
    BetweenUndoDeletes,
}

impl Crashes {
    #[cfg(test)]
    pub fn set(&self, point: CrashPoint) {
        *self.at.lock().unwrap() = Some(point);
    }

    /// Whether an operation stopped at the crash point set.
    #[cfg(test)]
    pub fn hit(&self) -> bool {
        self.hit.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether `point` is the crash point set. The caller then drops what it holds open and
    /// calls `stop`.
    #[cfg(test)]
    pub fn at(&self, point: CrashPoint) -> bool {
        *self.at.lock().unwrap() == Some(point)
    }

    #[cfg(not(test))]
    pub fn at(&self, _point: CrashPoint) -> bool {
        false
    }

    /// Never returns.
    pub async fn stop(&self) -> Infallible {
        #[cfg(test)]
        self.hit.store(true, std::sync::atomic::Ordering::SeqCst);
        std::future::pending().await
    }

    /// Stops for good at `point` when it is the crash point set.
    pub async fn check(&self, point: CrashPoint) {
        if self.at(point) {
            match self.stop().await {}
        }
    }
}

/// Moves the run to `undoing`, in one master transaction that first rejects its proposals still
/// pending. Refused unless the run is `done`, `cancelled`, `awaiting_review` or `undo_partial`
/// (Finish undo), and while any of its items is in flight: an item's write could land after
/// undo removed the run's rows.
pub async fn begin(master: &Pool<Sqlite>, run_id: &str) -> Result<(), String> {
    let mut tx = master
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(|e| e.to_string())?;
    let (status, in_flight): (String, i64) = sqlx::query_as(
        "SELECT status, (SELECT COUNT(*) FROM tag_run_items
                         WHERE run_id = tag_runs.id AND status = 'running')
         FROM tag_runs WHERE id = ?",
    )
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| e.to_string())?
    .ok_or("The run no longer exists")?;
    if !["done", "cancelled", "awaiting_review", "undo_partial"].contains(&status.as_str()) {
        return Err("Only a finished, cancelled or partly undone run can be undone".into());
    }
    if in_flight > 0 {
        return Err("Wait for the requests still in flight to finish".into());
    }
    let start = async {
        sqlx::query(
            "UPDATE tag_run_proposals SET status = 'rejected'
             WHERE run_id = ? AND status = 'pending'",
        )
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE tag_runs SET status = 'undoing', pause_reason = NULL WHERE id = ?")
            .bind(run_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    };
    start.await.map_err(|e| e.to_string())
}

/// What undo did.
#[derive(Debug)]
pub struct Finished {
    /// `undone`, or `undo_partial` with its reason in `pause_reason`.
    pub status: &'static str,
    /// Assets whose tags changed in an open library.
    pub changed_assets: Vec<String>,
    /// It deleted tags, so the catalog changed.
    pub tags_deleted: bool,
}

/// The steps of an `undoing` run, for Undo and Finish undo alike:
///
/// 1. For each library holding any of the run's rows (`pending` or `applied`): an open library
///    has those pairs deleted from its `asset_tags`, then the matching master rows, in batches.
///    An offline library's rows stay. A library removed from OpenDAM has nothing left to undo,
///    so its rows go.
/// 2. With every row gone and every registered library open, the tags the run created that
///    nothing uses any more are deleted through the tag-delete path. The run is `undone`.
/// 3. Otherwise it is `undo_partial`, with the reason. Created tags stay while a registered
///    library is offline: its mirror still holds them, and opening it would add them straight
///    back to master.
///
/// A crash between a library delete and its master delete leaves master rows for pairs the
/// library no longer has: replay skips them, and the next Finish undo removes them.
pub async fn finish(db: &Database, run_id: &str, crashes: &Crashes) -> Finished {
    let mut changed_assets = Vec::new();
    let mut tags_deleted = false;
    let reason = match remove(db, run_id, crashes, &mut changed_assets, &mut tags_deleted).await {
        Ok(reason) => reason,
        Err(e) => Some(format!("Undo stopped: {e}")),
    };
    let master = &db.master_pool;
    let (status, logged) = match &reason {
        None => ("undone", "Undid"),
        Some(_) => ("undo_partial", "Partly undid"),
    };
    let ended = sqlx::query(
        "UPDATE tag_runs SET status = ?, pause_reason = ? WHERE id = ? AND status = 'undoing'",
    )
    .bind(status)
    .bind(&reason)
    .bind(run_id)
    .execute(master)
    .await;
    match ended {
        Ok(_) => tracing::info!(
            "{logged} auto-tag run {run_id}{}",
            match &reason {
                Some(reason) => format!(": {reason}"),
                None => String::new(),
            }
        ),
        // Left `undoing`, which the next startup turns into `undo_partial`.
        Err(e) => tracing::error!("Could not end the undo of auto-tag run {run_id}: {e}"),
    }
    Finished {
        status,
        changed_assets,
        tags_deleted,
    }
}

/// Steps 1 and 2. Returns why the run is only partly undone, or None once it is undone.
async fn remove(
    db: &Database,
    run_id: &str,
    crashes: &Crashes,
    changed_assets: &mut Vec<String>,
    tags_deleted: &mut bool,
) -> sqlx::Result<Option<String>> {
    let master = &db.master_pool;
    let registered: BTreeMap<String, String> =
        sqlx::query_as("SELECT id, name FROM libraries ORDER BY id")
            .fetch_all(master)
            .await?
            .into_iter()
            .collect();
    let open: HashMap<String, LibraryHandle> = db
        .get_all_libraries()
        .await
        .into_iter()
        .map(|lib| (lib.id.clone(), lib))
        .collect();

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT library_id, asset_id, tag_id FROM tag_run_applied WHERE run_id = ?
         ORDER BY library_id, asset_id, tag_id",
    )
    .bind(run_id)
    .fetch_all(master)
    .await?;
    let mut by_library: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (library_id, asset_id, tag_id) in rows {
        by_library
            .entry(library_id)
            .or_default()
            .push((asset_id, tag_id));
    }

    let mut left = Vec::new();
    for (library_id, pairs) in by_library {
        if let Some(lib) = open.get(&library_id) {
            match remove_pairs(db, lib, run_id, &pairs, crashes).await {
                Ok(assets) => changed_assets.extend(assets),
                Err(e) => {
                    let n = remaining(master, run_id, &library_id).await?;
                    left.push(format!(
                        "Library {}: {} not removed ({e})",
                        lib.name,
                        plural(n, "tag")
                    ));
                }
            }
        } else if let Some(name) = registered.get(&library_id) {
            left.push(format!(
                "Library {name} offline: {} not removed",
                plural(pairs.len() as i64, "tag")
            ));
        } else {
            sqlx::query("DELETE FROM tag_run_applied WHERE run_id = ? AND library_id = ?")
                .bind(run_id)
                .bind(&library_id)
                .execute(master)
                .await?;
        }
    }
    if !left.is_empty() {
        return Ok(Some(left.join("; ")));
    }

    let created: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT p.final_tag_id FROM tag_run_proposals p JOIN tags t ON t.id = p.final_tag_id
         WHERE p.run_id = ? AND p.created_tag = 1",
    )
    .bind(run_id)
    .fetch_all(master)
    .await?;
    if created.is_empty() {
        return Ok(None);
    }
    let offline: Vec<&str> = registered
        .iter()
        .filter(|(id, _)| !open.contains_key(*id))
        .map(|(_, name)| name.as_str())
        .collect();
    if !offline.is_empty() {
        return Ok(Some(format!(
            "Created tags kept until {} {} back",
            offline.join(" and "),
            if offline.len() == 1 { "is" } else { "are" }
        )));
    }
    for tag_id in unused(db, run_id, created).await? {
        if let Err(e) = delete_tag_in(db, &tag_id).await {
            return Ok(Some(format!("Created tags kept: {e}")));
        }
        *tags_deleted = true;
    }
    Ok(None)
}

/// Deletes the run's pairs from an open library's `asset_tags`, then their master rows, a batch
/// at a time. Deleting a `pending` pair the library never got is a no-op. Returns the assets
/// whose tags changed.
async fn remove_pairs(
    db: &Database,
    lib: &LibraryHandle,
    run_id: &str,
    pairs: &[(String, String)],
    crashes: &Crashes,
) -> sqlx::Result<Vec<String>> {
    let mut changed = Vec::new();
    for chunk in pairs.chunks(BATCH) {
        let mut tx = lib.pool.begin().await?;
        for (asset_id, tag_id) in chunk {
            let deleted = sqlx::query("DELETE FROM asset_tags WHERE asset_id = ? AND tag_id = ?")
                .bind(asset_id)
                .bind(tag_id)
                .execute(&mut *tx)
                .await?;
            if deleted.rows_affected() > 0 {
                changed.push(asset_id.clone());
            }
        }
        tx.commit().await?;
        crashes.check(CrashPoint::BetweenUndoDeletes).await;

        let mut tx = db.master_pool.begin().await?;
        for (asset_id, tag_id) in chunk {
            sqlx::query(
                "DELETE FROM tag_run_applied WHERE run_id = ? AND asset_id = ? AND tag_id = ?",
            )
            .bind(run_id)
            .bind(asset_id)
            .bind(tag_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }
    changed.dedup();
    Ok(changed)
}

async fn remaining(master: &Pool<Sqlite>, run_id: &str, library_id: &str) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM tag_run_applied WHERE run_id = ? AND library_id = ?")
        .bind(run_id)
        .bind(library_id)
        .fetch_one(master)
        .await
}

/// The tags among `created` that nothing uses any more: no `asset_tags` row in an open library,
/// no other run's write waiting for replay, and no child tag that stays. Deleting a parent
/// someone put their own tag under would move that tag to the top level.
async fn unused(db: &Database, run_id: &str, created: Vec<String>) -> sqlx::Result<Vec<String>> {
    let mut unused: HashSet<String> = created.into_iter().collect();
    for lib in db.get_all_libraries().await {
        let used: Vec<String> = sqlx::query_scalar("SELECT DISTINCT tag_id FROM asset_tags")
            .fetch_all(&lib.pool)
            .await?;
        for tag_id in used {
            unused.remove(&tag_id);
        }
    }
    let waiting: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT tag_id FROM tag_run_applied WHERE run_id != ? AND state = 'pending'",
    )
    .bind(run_id)
    .fetch_all(&db.master_pool)
    .await?;
    for tag_id in waiting {
        unused.remove(&tag_id);
    }
    let children: Vec<(String, String)> =
        sqlx::query_as("SELECT id, parent_id FROM tags WHERE parent_id IS NOT NULL")
            .fetch_all(&db.master_pool)
            .await?;
    loop {
        let kept_parents: Vec<String> = children
            .iter()
            .filter(|(id, parent)| unused.contains(parent) && !unused.contains(id))
            .map(|(_, parent)| parent.clone())
            .collect();
        if kept_parents.is_empty() {
            break;
        }
        for parent in kept_parents {
            unused.remove(&parent);
        }
    }
    let mut unused: Vec<String> = unused.into_iter().collect();
    unused.sort();
    Ok(unused)
}

fn plural(n: i64, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autotag::engine::{Engine, RunScope};
    use crate::autotag::fake::FakeProvider;
    use crate::autotag::provider::Mode;
    use crate::autotag::store::{self, UNDO_INTERRUPTED};
    use crate::db::{
        insert_asset, insert_tag, memory_database, memory_library, memory_library_pool,
    };
    use std::future::Future;
    use std::sync::Arc;
    use std::time::Duration;

    /// An engine over `db` with the fake provider, which tags an asset named `rock-moss-a1`
    /// with the catalog's Rock and, outside Existing only, proposes Moss. Started, so it has
    /// run the startup path: restart recovery, then replay.
    async fn started_engine(db: &Arc<Database>) -> Arc<Engine> {
        let engine = Engine::new(
            db.clone(),
            vec![Arc::new(FakeProvider::new(false, Duration::ZERO))],
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        );
        engine.start().await;
        engine
    }

    async fn eventually<F: Future<Output = bool>>(what: &str, check: impl Fn() -> F) {
        for _ in 0..500 {
            if check().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never happened: {what}");
    }

    async fn count(pool: &Pool<Sqlite>, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    async fn status(db: &Database, run_id: &str) -> (String, Option<String>) {
        sqlx::query_as("SELECT status, pause_reason FROM tag_runs WHERE id = ?")
            .bind(run_id)
            .fetch_one(&db.master_pool)
            .await
            .unwrap()
    }

    /// Starts a run and waits until it ended `want` with every write it recorded applied.
    async fn run(
        engine: &Engine,
        db: &Database,
        assets: &[&str],
        mode: Mode,
        want: &str,
    ) -> String {
        let scope = RunScope::Assets {
            asset_ids: assets.iter().map(|a| a.to_string()).collect(),
        };
        let run_id = engine.start_run(scope, "fake", mode).await.unwrap().run.id;
        eventually("the run ends with its writes applied", || async {
            status(db, &run_id).await.0 == want
                && count(
                    &db.master_pool,
                    "SELECT COUNT(*) FROM tag_run_applied WHERE state = 'pending'",
                )
                .await
                    == 0
        })
        .await;
        run_id
    }

    /// Every asset's tags and every tag definition, in master and in each library.
    async fn snapshot(db: &Database, libraries: &[&Pool<Sqlite>]) -> Vec<Vec<String>> {
        let tags =
            "SELECT id || ' ' || name || ' ' || COALESCE(parent_id, '-') FROM tags ORDER BY id";
        let mut snapshot = vec![sqlx::query_scalar(tags)
            .fetch_all(&db.master_pool)
            .await
            .unwrap()];
        for pool in libraries {
            snapshot.push(sqlx::query_scalar(tags).fetch_all(*pool).await.unwrap());
            snapshot.push(
                sqlx::query_scalar(
                    "SELECT asset_id || ' ' || tag_id FROM asset_tags ORDER BY asset_id, tag_id",
                )
                .fetch_all(*pool)
                .await
                .unwrap(),
            );
        }
        snapshot
    }

    async fn asset_tags(pool: &Pool<Sqlite>) -> Vec<(String, String)> {
        sqlx::query_as("SELECT asset_id, tag_id FROM asset_tags ORDER BY asset_id, tag_id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn moss_count(pool: &Pool<Sqlite>) -> i64 {
        count(pool, "SELECT COUNT(*) FROM tags WHERE name = 'Moss'").await
    }

    /// Libraries registered in master and open, with Rock in the catalog and every mirror.
    async fn libraries(names: &[&str], assets: &[&[&str]]) -> (Arc<Database>, Vec<Pool<Sqlite>>) {
        let mut pools = Vec::new();
        for library_assets in assets {
            let pool = memory_library_pool().await;
            for asset in *library_assets {
                insert_asset(&pool, asset).await;
            }
            insert_tag(&pool, "t-rock", "Rock").await;
            pools.push(pool);
        }
        let db = memory_database(
            names
                .iter()
                .zip(&pools)
                .map(|(name, pool)| memory_library(name, pool.clone()))
                .collect(),
        )
        .await;
        insert_tag(&db.master_pool, "t-rock", "Rock").await;
        for name in names {
            sqlx::query(
                "INSERT INTO libraries (id, name, root_paths, created_at) VALUES (?, ?, '[]', 0)",
            )
            .bind(name)
            .bind(name)
            .execute(&db.master_pool)
            .await
            .unwrap();
        }
        (Arc::new(db), pools)
    }

    /// Opens `name` again over `pool` the way adding its folder does: the writes waiting for it
    /// are read first, then it opens, then they are replayed.
    async fn reopen(engine: &Engine, db: &Database, name: &str, pool: &Pool<Sqlite>) {
        let waiting = engine.waiting_writes(name).await.unwrap();
        reopen_with(engine, db, name, pool, waiting).await;
    }

    async fn reopen_with(
        engine: &Engine,
        db: &Database,
        name: &str,
        pool: &Pool<Sqlite>,
        waiting: Vec<store::PendingPair>,
    ) {
        let lib = memory_library(name, pool.clone());
        db.libraries.write().await.insert(name.to_string(), lib);
        db.sync_open_libraries().await;
        engine.library_opened(waiting).await;
    }

    #[tokio::test]
    async fn undo_removes_exactly_the_runs_pairs_and_the_tags_it_created() {
        let (db, pools) = libraries(
            &["A"],
            &[&["rock-a1", "rock-moss-a2", "rock-b1", "fern-a3"]],
        )
        .await;
        let a = &pools[0];
        insert_tag(&db.master_pool, "t-fern", "Fern").await;
        insert_tag(a, "t-fern", "Fern").await;
        for (asset, tag) in [("rock-a1", "t-rock"), ("fern-a3", "t-fern")] {
            sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES (?, ?)")
                .bind(asset)
                .bind(tag)
                .execute(a)
                .await
                .unwrap();
        }
        let engine = started_engine(&db).await;
        let before = snapshot(&db, &[a]).await;
        let run_id = run(
            &engine,
            &db,
            &["rock-a1", "rock-moss-a2"],
            Mode::CreateFreely,
            "done",
        )
        .await;
        // Another run's tag, which this undo must leave.
        run(&engine, &db, &["rock-b1"], Mode::ExistingOnly, "done").await;
        assert_eq!(moss_count(&db.master_pool).await, 1);
        assert_eq!(moss_count(a).await, 1);
        assert_eq!(asset_tags(a).await.len(), 5);

        assert_eq!(engine.undo_run(&run_id).await.unwrap(), "undone");

        let mut expected = before;
        expected[2].push("rock-b1 t-rock".into());
        expected[2].sort();
        assert_eq!(
            snapshot(&db, &[a]).await,
            expected,
            "only the run's pairs went, and Moss with them; rock-a1's own Rock stays"
        );
        assert_eq!(
            count(&db.master_pool, "SELECT COUNT(*) FROM tag_run_applied").await,
            1,
            "only the other run's row is left"
        );
        assert_eq!(status(&db, &run_id).await, ("undone".into(), None));
    }

    #[tokio::test]
    async fn a_created_tag_still_in_use_and_a_created_parent_with_a_kept_child_stay() {
        let (db, pools) = libraries(&["A"], &[&["rock-moss-a1", "fern-a2"]]).await;
        let a = &pools[0];
        let engine = started_engine(&db).await;
        let run_id = run(&engine, &db, &["rock-moss-a1"], Mode::CreateFreely, "done").await;
        let moss: String = sqlx::query_scalar("SELECT id FROM tags WHERE name = 'Moss'")
            .fetch_one(&db.master_pool)
            .await
            .unwrap();
        // You put your own tag under Moss.
        sqlx::query("INSERT INTO tags (id, name, parent_id) VALUES ('t-lichen', 'Lichen', ?)")
            .bind(&moss)
            .execute(&db.master_pool)
            .await
            .unwrap();

        assert_eq!(engine.undo_run(&run_id).await.unwrap(), "undone");
        assert_eq!(
            moss_count(&db.master_pool).await,
            1,
            "Lichen keeps its parent"
        );
        assert!(asset_tags(a).await.is_empty());

        // A second run creates Moss again, and you tag another asset with it by hand.
        for tag in ["t-lichen", moss.as_str()] {
            delete_tag_in(&db, tag).await.unwrap();
        }
        let run_id = run(&engine, &db, &["rock-moss-a1"], Mode::CreateFreely, "done").await;
        sqlx::query(
            "INSERT INTO asset_tags (asset_id, tag_id)
             SELECT 'fern-a2', id FROM tags WHERE name = 'Moss'",
        )
        .execute(a)
        .await
        .unwrap();

        assert_eq!(engine.undo_run(&run_id).await.unwrap(), "undone");
        assert_eq!(
            moss_count(&db.master_pool).await,
            1,
            "fern-a2 still uses it"
        );
        assert_eq!(asset_tags(a).await.len(), 1);
    }

    #[tokio::test]
    async fn undo_rejects_pending_proposals_and_cancels_a_paused_run_first() {
        let (db, pools) = libraries(&["A"], &[&["rock-moss-a1", "rock-a2"]]).await;
        let engine = started_engine(&db).await;
        let run_id = run(
            &engine,
            &db,
            &["rock-moss-a1"],
            Mode::PreferExisting,
            "awaiting_review",
        )
        .await;

        assert_eq!(engine.undo_run(&run_id).await.unwrap(), "undone");
        assert!(asset_tags(&pools[0]).await.is_empty());
        let proposals: Vec<String> = sqlx::query_scalar("SELECT status FROM tag_run_proposals")
            .fetch_all(&db.master_pool)
            .await
            .unwrap();
        assert_eq!(proposals, ["rejected"]);

        // A paused run with an asset still queued.
        sqlx::query(
            "INSERT INTO tag_runs (id, label, provider, mode, status, created_at)
             VALUES ('paused-run', 'run', 'fake', 'existing_only', 'paused', 0)",
        )
        .execute(&db.master_pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO tag_run_items (run_id, asset_id, library_id) VALUES ('paused-run', 'rock-a2', 'A')",
        )
        .execute(&db.master_pool)
        .await
        .unwrap();
        assert_eq!(engine.undo_run("paused-run").await.unwrap(), "undone");
        let item: String =
            sqlx::query_scalar("SELECT status FROM tag_run_items WHERE run_id = 'paused-run'")
                .fetch_one(&db.master_pool)
                .await
                .unwrap();
        assert_eq!(item, "skipped", "cancelled first");
    }

    #[tokio::test]
    async fn undo_is_refused_for_a_run_still_going_or_with_an_item_in_flight() {
        let (db, _pools) = libraries(&["A"], &[&["rock-a1"]]).await;
        let engine = started_engine(&db).await;
        for (id, status, item) in [
            ("running-run", "running", "queued"),
            ("queued-run", "queued", "queued"),
            ("undone-run", "undone", "done"),
            ("in-flight-run", "cancelled", "running"),
        ] {
            sqlx::query(
                "INSERT INTO tag_runs (id, label, provider, mode, status, created_at)
                 VALUES (?, 'run', 'other', 'existing_only', ?, 0)",
            )
            .bind(id)
            .bind(status)
            .execute(&db.master_pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO tag_run_items (run_id, asset_id, library_id, status)
                 VALUES (?, 'rock-a1', 'A', ?)",
            )
            .bind(id)
            .bind(item)
            .execute(&db.master_pool)
            .await
            .unwrap();
            let error = engine.undo_run(id).await.unwrap_err();
            assert!(
                error.contains(if item == "running" {
                    "in flight"
                } else {
                    "can be undone"
                }),
                "{id}: {error}"
            );
            assert_eq!(
                super::super::store::run_status(&db.master_pool, id)
                    .await
                    .unwrap()
                    .unwrap(),
                status
            );
        }
    }

    /// A Create freely run over two assets of library A stops for good at `point`, the app
    /// restarts on the same databases, and the run is undone: every asset's tags and the
    /// catalog are as before the run.
    async fn crash_then_undo(point: CrashPoint) {
        let (db, pools) = libraries(&["A"], &[&["rock-moss-a1", "rock-moss-a2", "a3"]]).await;
        let a = &pools[0];
        insert_tag(&db.master_pool, "t-tree", "Tree").await;
        insert_tag(a, "t-tree", "Tree").await;
        sqlx::query("INSERT INTO asset_tags (asset_id, tag_id) VALUES ('rock-moss-a1', 't-tree')")
            .execute(a)
            .await
            .unwrap();
        let before = snapshot(&db, &[a]).await;
        let crashed = started_engine(&db).await;
        crashed.crashes().set(point);
        let scope = RunScope::Assets {
            asset_ids: vec!["rock-moss-a1".into(), "rock-moss-a2".into()],
        };
        let run_id = crashed
            .start_run(scope, "fake", Mode::CreateFreely)
            .await
            .unwrap()
            .run
            .id;
        eventually("an item reaches the crash point", || async {
            crashed.crashes().hit()
        })
        .await;
        match point {
            CrashPoint::AfterTagInsert => assert_eq!(
                moss_count(&db.master_pool).await,
                0,
                "the tag never commits without its owner"
            ),
            _ => assert_eq!(moss_count(&db.master_pool).await, 1),
        }

        let engine = started_engine(&db).await;
        assert_eq!(status(&db, &run_id).await.0, "paused");
        assert_eq!(engine.undo_run(&run_id).await.unwrap(), "undone");

        assert_eq!(snapshot(&db, &[a]).await, before, "{point:?}");
        assert_eq!(
            count(&db.master_pool, "SELECT COUNT(*) FROM tag_run_applied").await,
            0
        );
    }

    #[tokio::test]
    async fn a_crash_after_the_intent_commit_is_undone_to_the_pre_run_state() {
        crash_then_undo(CrashPoint::AfterIntent).await;
    }

    #[tokio::test]
    async fn a_crash_after_the_library_commit_is_undone_to_the_pre_run_state() {
        crash_then_undo(CrashPoint::AfterLibraryCommit).await;
    }

    #[tokio::test]
    async fn a_crash_after_a_create_freely_tags_master_insert_is_undone_to_the_pre_run_state() {
        crash_then_undo(CrashPoint::AfterTagInsert).await;
    }

    #[tokio::test]
    async fn an_offline_library_keeps_its_rows_until_finish_undo_and_replay_never_applies_them() {
        let (db, pools) = libraries(
            &["A", "B"],
            &[&["rock-moss-a1"], &["rock-moss-b1", "rock-b2"]],
        )
        .await;
        let (a, b) = (&pools[0], &pools[1]);
        let engine = started_engine(&db).await;
        let run_id = run(
            &engine,
            &db,
            &["rock-moss-a1", "rock-moss-b1"],
            Mode::CreateFreely,
            "done",
        )
        .await;
        // B also holds a write still waiting for replay.
        sqlx::query(
            "INSERT INTO tag_run_applied (run_id, asset_id, library_id, tag_id)
             VALUES (?, 'rock-b2', 'B', 't-rock')",
        )
        .bind(&run_id)
        .execute(&db.master_pool)
        .await
        .unwrap();
        // Read as B's folder comes back, just before the undo starts.
        let read_before_undo = engine.waiting_writes("B").await.unwrap();
        assert_eq!(read_before_undo.len(), 1);
        db.unregister_library("B").await.unwrap();
        let b_before = asset_tags(b).await;

        assert_eq!(engine.undo_run(&run_id).await.unwrap(), "undo_partial");

        assert_eq!(
            status(&db, &run_id).await.1.as_deref(),
            Some("Library B offline: 3 tags not removed")
        );
        assert!(asset_tags(a).await.is_empty(), "A is restored");
        assert_eq!(moss_count(a).await, 1, "Moss stays while B holds rows");
        assert_eq!(moss_count(&db.master_pool).await, 1);
        assert_eq!(
            count(
                &db.master_pool,
                "SELECT COUNT(*) FROM tag_run_applied WHERE library_id = 'B'"
            )
            .await,
            3,
            "B's rows are kept"
        );

        reopen_with(&engine, &db, "B", b, read_before_undo).await;
        assert_eq!(asset_tags(b).await, b_before, "replay leaves B as it was");
        db.unregister_library("B").await.unwrap();
        reopen(&engine, &db, "B", b).await;
        assert_eq!(asset_tags(b).await, b_before);

        assert_eq!(engine.undo_run(&run_id).await.unwrap(), "undone");
        assert!(asset_tags(b).await.is_empty(), "B is restored");
        for pool in [&db.master_pool, a, b] {
            assert_eq!(moss_count(pool).await, 0, "only now is Moss deleted");
        }
        assert_eq!(
            count(&db.master_pool, "SELECT COUNT(*) FROM tag_run_applied").await,
            0
        );
    }

    #[tokio::test]
    async fn a_crash_between_the_undo_deletes_leaves_the_library_restored_and_the_run_partly_undone(
    ) {
        let (db, pools) = libraries(&["A"], &[&["rock-a1", "rock-a2"]]).await;
        let a = &pools[0];
        let crashed = started_engine(&db).await;
        let run_id = run(
            &crashed,
            &db,
            &["rock-a1", "rock-a2"],
            Mode::ExistingOnly,
            "done",
        )
        .await;
        crashed.crashes().set(CrashPoint::BetweenUndoDeletes);
        let undoing = crashed.clone();
        let id = run_id.clone();
        tokio::spawn(async move { undoing.undo_run(&id).await });
        eventually("undo reaches the crash point", || async {
            crashed.crashes().hit()
        })
        .await;
        assert!(asset_tags(a).await.is_empty());
        assert_eq!(
            count(&db.master_pool, "SELECT COUNT(*) FROM tag_run_applied").await,
            2,
            "the master rows are still there"
        );

        let engine = started_engine(&db).await;
        assert_eq!(
            status(&db, &run_id).await,
            ("undo_partial".into(), Some(UNDO_INTERRUPTED.into()))
        );
        assert!(asset_tags(a).await.is_empty(), "replay skipped the rows");

        assert_eq!(engine.undo_run(&run_id).await.unwrap(), "undone");
        assert_eq!(
            count(&db.master_pool, "SELECT COUNT(*) FROM tag_run_applied").await,
            0
        );
    }

    #[tokio::test]
    async fn created_tags_stay_while_a_library_with_none_of_the_runs_rows_is_offline() {
        let (db, pools) = libraries(&["A", "C"], &[&["rock-moss-a1"], &[]]).await;
        let (a, c) = (&pools[0], &pools[1]);
        let engine = started_engine(&db).await;
        let run_id = run(&engine, &db, &["rock-moss-a1"], Mode::CreateFreely, "done").await;
        assert_eq!(moss_count(c).await, 1, "mirrored to C");
        db.unregister_library("C").await.unwrap();

        assert_eq!(engine.undo_run(&run_id).await.unwrap(), "undo_partial");
        assert_eq!(
            status(&db, &run_id).await.1.as_deref(),
            Some("Created tags kept until C is back")
        );
        assert!(asset_tags(a).await.is_empty());
        assert_eq!(moss_count(&db.master_pool).await, 1);

        reopen(&engine, &db, "C", c).await;
        assert_eq!(engine.undo_run(&run_id).await.unwrap(), "undone");
        for pool in [&db.master_pool, a, c] {
            assert_eq!(
                moss_count(pool).await,
                0,
                "deleted from master and every mirror"
            );
        }
        db.unregister_library("C").await.unwrap();
        reopen(&engine, &db, "C", c).await;
        assert_eq!(
            moss_count(&db.master_pool).await,
            0,
            "C doesn't bring it back"
        );
    }
}
