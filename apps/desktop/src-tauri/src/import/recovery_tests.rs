//! Swap recovery (plan pack-import, Flow "Swap recovery", A11 and A14): builds crashed at
//! injected points (a panic, which leaves the disk and the records as a killed process would),
//! then a restart, a refresh, a reconnect or a second `Database` on the same library folder,
//! standing for another machine. Each row of both recovery tables has its own case.
//!
//! The tests sit in a `mod tests` block, as in every other file, so source scans that stop there
//! (`structure::tests::metadata_writers`) read none of them.

mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::super::rebuild_tests::tests::{add, in_pack, reached, toggle, Rows};
    use crate::db::{self, Database, LibraryHandle};
    use crate::fs_swap;
    use crate::import::engine::tests::{setup_in, Setup, TARGET};
    use crate::import::engine::*;
    use crate::import::unitypackage::tests::on_disk;
    use crate::structure;
    use crate::watcher;

    /// What the engine records for a build task that panicked.
    const STOPPED: &str = "The run stopped unexpectedly";
    /// A path only the previous build has in the later-build cases ("Old.png").
    const OLD: &str = "Textures/T_Log_D.png";
    /// A material whose `.meta` the previous build excludes and the failed build has (A14).
    const MAT: &str = "Materials/M_LogBark.mat";
    /// The nested package's `Art` folder, below the pack root once flattened.
    const HOUSES_ART: &str = "Houses/Built-In/Built-In_Pack/Pack/Art";

    /// A library on disk, so it can be opened again, with an engine started over it.
    async fn disk() -> Setup {
        setup_in(true, true).await
    }

    fn root(s: &Setup) -> String {
        s.dir.path().to_string_lossy().into_owned()
    }

    /// The previous build of the later-build cases, and the edit of the next one. The golden
    /// pack is built again with `M_LogBark.mat.meta` excluded, so its material is visible, and
    /// with the nested package's `M_Rock.mat` excluded. Three rows are tagged: `T_Rock.png`,
    /// which the next build moves to `Art/T_Rock.png` (a carried row); `Textures/T_Log_D.png`,
    /// which the next build excludes (a previous-only row); and `Readme.txt`, which both have.
    /// The next build turns `hoist` off, excludes `T_Log_D.png` and brings back the `.meta` and
    /// `M_Rock.mat`, whose row its index inserts. Returns the pack id.
    async fn later(s: &Setup) -> String {
        let pack_id = s.import_golden().await;
        let meta = s.source_of(&format!("{MAT}.meta")).await;
        let rock_mat = s
            .source_of(&format!("{HOUSES_ART}/Materials/M_Rock.mat"))
            .await;
        s.edit_steps(&pack_id, |steps| {
            add(steps, PlaceStep::Exclude { source: meta });
            add(steps, PlaceStep::Exclude { source: rock_mat });
        })
        .await;
        s.rebuild_until(&pack_id, "done").await;
        s.tag("T_Rock.png", "rock").await;
        s.tag(OLD, "old").await;
        s.tag("Readme.txt", "readme").await;
        let old = s.source_of(OLD).await;
        s.edit_steps(&pack_id, |steps| {
            for step in steps.iter_mut().filter(|step| step.kind == "exclude") {
                step.enabled = false;
            }
            toggle(steps, "hoist", false);
            add(steps, PlaceStep::Exclude { source: old });
        })
        .await;
        pack_id
    }

    /// The library as a build found it, for a rollback to restore.
    struct Before {
        build: Option<String>,
        rows: Rows,
        tree: Vec<String>,
    }

    async fn before(s: &Setup) -> Before {
        Before {
            build: s.current_build().await,
            rows: s.rows().await,
            tree: on_disk(&s.live()),
        }
    }

    /// The library is as `before` had it: the pack folder, every row with its id, path and tags,
    /// and the committed build, with nothing of the failed build left, and each material's role
    /// by the restored manifest: `M_LogBark.mat`, whose `.meta` only the failed build had, stays
    /// visible (A14).
    async fn restored(s: &Setup, before: &Before) {
        assert_eq!(on_disk(&s.live()), before.tree);
        assert_eq!(s.current_build().await, before.build);
        assert_eq!(s.pack().await.0, "built");
        assert_eq!(s.rows().await, before.rows);
        assert_eq!(
            s.count(&format!(
                "SELECT COUNT(*) FROM assets WHERE file_path LIKE '{TARGET}/Art/%'"
            ))
            .await,
            0,
            "no row of a path only the failed build had"
        );
        s.settled().await;
        s.tree_untouched().await;
        roles_match_manifest(s).await;
        assert_eq!(role(s, MAT).await, None, "the restored material is visible");
    }

    /// A first build left nothing: no pack folder, no row in it, no build, no records.
    async fn nothing_built(s: &Setup) {
        assert!(!s.live().exists());
        assert_eq!(
            s.count(&format!(
                "SELECT COUNT(*) FROM assets WHERE file_path LIKE '{TARGET}/%'"
            ))
            .await,
            0
        );
        assert_eq!(s.pack().await.1, None);
        assert_eq!(s.pack().await.2, None);
        assert_eq!(s.count("SELECT COUNT(*) FROM import_outputs").await, 0);
        assert_eq!(s.count("SELECT COUNT(*) FROM import_moves").await, 0);
        s.no_leftovers();
        s.tree_untouched().await;
    }

    /// The support role of the row at `path` in the pack.
    async fn role(s: &Setup, path: &str) -> Option<String> {
        sqlx::query_scalar("SELECT support_role FROM assets WHERE file_path = ?")
            .bind(in_pack(path))
            .fetch_one(&s.lib.pool)
            .await
            .unwrap()
    }

    /// Every `.mat` in the pack is a hidden `unity` file exactly when its `.mat.meta` is in the
    /// current manifest (A14).
    async fn roles_match_manifest(s: &Setup) {
        let mats: Vec<(String, Option<String>, i64)> = sqlx::query_as(
            "SELECT a.file_path, a.support_role, EXISTS (SELECT 1 FROM import_outputs o
                 JOIN import_packs p ON p.id = o.pack_id AND o.build_id = p.current_build
                 WHERE o.path = a.file_path || '.meta')
             FROM assets a WHERE a.extension = 'mat'",
        )
        .fetch_all(&s.lib.pool)
        .await
        .unwrap();
        for (path, role, meta) in mats {
            let expected = (meta == 1).then(|| "unity".to_string());
            assert_eq!(role, expected, "{path}");
        }
    }

    /// Waits for run `run_id`'s build to crash, then leaves its record as a killed process does:
    /// `running`, owned by a process that is gone, with a heartbeat long stopped.
    async fn killed(s: &Setup, run_id: &str) {
        let run = s.wait_for(run_id, "failed").await;
        assert_eq!(run.error.as_deref(), Some(STOPPED));
        sqlx::query(
            "UPDATE import_runs SET status = 'running', error = NULL, finished_at = NULL,
             owner_started = 1, heartbeat_at = 0 WHERE id = ?",
        )
        .bind(run_id)
        .execute(&s.lib.pool)
        .await
        .unwrap();
    }

    /// A rebuild of `pack_id` killed at the crash point the test set. Returns its run's id.
    async fn rebuild_killed(s: &Setup, pack_id: &str) -> String {
        let run = s.rebuild(pack_id).await;
        killed(s, &run.id).await;
        run.id
    }

    /// The golden pack's first build, killed at the crash point the test set.
    async fn import_killed(s: &Setup) -> String {
        let run = s.engine.start_import(s.golden_request()).await.unwrap();
        killed(s, &run.id).await;
        run.id
    }

    /// Opens the library again, as a restart or a reconnect does: `db::open_library_db`, which
    /// recovers before it registers anything.
    async fn reopen(s: &Setup) {
        db::open_library_db(&Default::default(), s.dir.path())
            .await
            .unwrap();
    }

    async fn refresh(s: &Setup) {
        watcher::index_paths(&s.engine.db, "lib", &[root(s)])
            .await
            .unwrap();
    }

    async fn status(s: &Setup, run_id: &str) -> String {
        records::run(&s.lib.pool, "lib", run_id)
            .await
            .unwrap()
            .status
    }

    /// The build id the pack's swap journal names.
    async fn swap_build(s: &Setup) -> String {
        sqlx::query_scalar("SELECT swap_build FROM import_packs")
            .fetch_one(&s.lib.pool)
            .await
            .unwrap()
    }

    fn beside(s: &Setup, tag: &str) -> Vec<std::path::PathBuf> {
        fs_swap::leftovers(&s.live(), tag).unwrap()
    }

    /// The pack is free again: no journal or run refuses its step edits and runs.
    async fn not_blocked(s: &Setup, pack_id: &str) {
        let steps = s.engine.pack_steps("lib", pack_id).await.unwrap().steps;
        s.engine
            .set_pack_steps("lib", pack_id, steps)
            .await
            .expect("the pack's step edits and runs aren't refused");
    }

    /// Another machine on the same library: a `Database` of its own, with its own `index_lock`,
    /// over its own pool to the library's DB file.
    struct Other {
        db: Arc<Database>,
        lib: LibraryHandle,
    }

    async fn other(s: &Setup) -> Other {
        let file = s.dir.path().join(".opendam/library.sqlite");
        let lib = LibraryHandle {
            id: "lib".into(),
            name: "lib".into(),
            root_path: s.dir.path().to_path_buf(),
            pool: db::connect_library_sqlite(&file).await.unwrap(),
        };
        let db = Arc::new(db::memory_database(vec![lib.clone()]).await);
        Other { db, lib }
    }

    /// Starts an engine over `db`, whose startup marks the runs of gone owners `interrupted`.
    fn start_engine(db: &Arc<Database>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(Engine::new(db.clone(), Arc::new(|_| {})).start())
    }

    // ── The `swap_state NULL` table ─────────────────────────────────────────

    #[tokio::test]
    async fn killed_while_staging_the_next_open_deletes_the_staging_and_its_rows() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let before = before(&s).await;
        s.engine.hooks().crash(Point::Staged);
        let run = rebuild_killed(&s, &pack_id).await;
        assert_eq!(
            on_disk(&s.live()),
            before.tree,
            "the live folder is as it was"
        );
        assert_eq!(beside(&s, "import").len(), 1);

        refresh(&s).await;
        assert_eq!(beside(&s, "import").len(), 1, "a refresh leaves it");

        reopen(&s).await;
        let engine = start_engine(&other(&s).await.db);
        s.wait_for(&run, "interrupted").await;
        engine.abort();
        restored(&s, &before).await;
    }

    #[tokio::test]
    async fn killed_late_in_a_rollback_the_next_open_deletes_the_failed_folder() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let before = before(&s).await;
        s.engine.hooks().fail(Point::Indexed);
        s.engine.hooks().crash(Point::RollbackCleared);
        rebuild_killed(&s, &pack_id).await;
        assert_eq!(s.pack().await.2, None, "its journal is cleared");
        assert_eq!(beside(&s, "failed").len(), 1);

        refresh(&s).await;
        assert_eq!(beside(&s, "failed").len(), 1, "a refresh leaves it");
        reopen(&s).await;
        restored(&s, &before).await;
    }

    #[tokio::test]
    async fn leftovers_stay_through_a_refresh_and_the_next_open_settles_and_deletes_them() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let previous = s.current_build().await;
        s.engine.hooks().crash(Point::Committed);
        rebuild_killed(&s, &pack_id).await;
        let committed = s.current_build().await;
        assert_ne!(committed, previous, "killed after the commit");
        assert_eq!(s.pack().await.2, None);
        // A staged build and a rollback that crashed late, both long dead.
        let (staged, failed) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let staging = fs_swap::create_staging(&s.live(), "import", &staged.to_string()).unwrap();
        std::fs::write(staging.join("Readme.txt"), "staged").unwrap();
        let failed = fs_swap::create_staging(&s.live(), "failed", &failed.to_string()).unwrap();
        std::fs::write(failed.join("Readme.txt"), "failed").unwrap();
        sqlx::query(
            "INSERT INTO import_outputs (pack_id, build_id, path, size, mtime_ns, sha256, origin,
             source_path) VALUES (?, ?, ?, 6, 0, '', 'extracted', '[\"x\"]')",
        )
        .bind(&pack_id)
        .bind(staged.to_string())
        .bind(in_pack("Readme.txt"))
        .execute(&s.lib.pool)
        .await
        .unwrap();
        let old_row = s.id_at(OLD).await;
        assert!(s.count("SELECT COUNT(*) FROM import_moves").await > 0);

        refresh(&s).await;
        for tag in ["prev", "import", "failed"] {
            assert_eq!(
                beside(&s, tag).len(),
                1,
                "a refresh leaves the {tag} folder"
            );
        }

        reopen(&s).await;
        assert_eq!(s.current_build().await, committed, "the new build stays");
        s.settled().await;
        assert!(
            !s.rows().await.contains_key(&old_row),
            "the previous-only row is pruned"
        );
        assert_eq!(
            role(&s, &format!("Art/{MAT}")).await.as_deref(),
            Some("unity")
        );
        roles_match_manifest(&s).await;
        s.tree_untouched().await;
    }

    #[tokio::test]
    async fn a_pack_with_a_live_run_keeps_its_staging_through_a_refresh_and_a_reconnect() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let pause = s.engine.hooks().pause(Point::Staged);
        let run = s.rebuild(&pack_id).await;
        reached(&pause).await;
        let staging = staged(&s).await;

        refresh(&s).await;
        assert_eq!(
            staged(&s).await,
            staging,
            "a refresh leaves the staging alone"
        );
        reopen(&s).await;
        assert_eq!(staged(&s).await, staging, "so does a reconnect");

        pause.release.notify_one();
        s.wait_for(&run.id, "done").await;
        s.settled().await;
        s.tree_untouched().await;
    }

    /// The one staging folder's files and the count of manifest rows.
    async fn staged(s: &Setup) -> (Vec<String>, i64) {
        let staging = beside(s, "import");
        assert_eq!(staging.len(), 1);
        (
            on_disk(&staging[0]),
            s.count("SELECT COUNT(*) FROM import_outputs").await,
        )
    }

    // ── The `swapping` table ────────────────────────────────────────────────

    #[tokio::test]
    async fn killed_after_the_journal_before_the_first_rename_deletes_the_staging_and_its_rows() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let before = before(&s).await;
        s.engine.hooks().crash(Point::Journaled);
        let run = rebuild_killed(&s, &pack_id).await;
        assert_eq!(s.pack().await.2.as_deref(), Some("swapping"));

        reopen(&s).await;
        assert_eq!(status(&s, &run).await, "interrupted");
        restored(&s, &before).await;

        let s = disk().await;
        s.engine.hooks().crash(Point::Journaled);
        let run = import_killed(&s).await;
        assert!(!s.live().exists() && beside(&s, "import").len() == 1);
        reopen(&s).await;
        assert_eq!(status(&s, &run).await, "interrupted");
        nothing_built(&s).await;
    }

    #[tokio::test]
    async fn a_failed_first_rename_killed_before_its_journal_clears_deletes_the_staging() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let before = before(&s).await;
        s.engine.hooks().fail_once(Point::MoveAside);
        s.engine.hooks().crash(Point::Unjournal);
        rebuild_killed(&s, &pack_id).await;
        assert_eq!(s.pack().await.2.as_deref(), Some("swapping"));
        assert_eq!(beside(&s, "import").len(), 1);

        reopen(&s).await;
        restored(&s, &before).await;
        not_blocked(&s, &pack_id).await;
    }

    #[tokio::test]
    async fn killed_between_the_two_renames_puts_the_previous_folder_back() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let before = before(&s).await;
        s.engine.hooks().crash(Point::Aside);
        rebuild_killed(&s, &pack_id).await;
        assert!(!s.live().exists());
        assert_eq!(
            (beside(&s, "import").len(), beside(&s, "prev").len()),
            (1, 1)
        );

        reopen(&s).await;
        restored(&s, &before).await;
    }

    #[tokio::test]
    async fn killed_after_the_first_rename_with_the_staging_gone_puts_the_previous_folder_back() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let before = before(&s).await;
        s.engine.hooks().crash(Point::Aside);
        rebuild_killed(&s, &pack_id).await;
        std::fs::remove_dir_all(&beside(&s, "import")[0]).unwrap();

        reopen(&s).await;
        restored(&s, &before).await;
    }

    #[tokio::test]
    async fn killed_after_the_swap_a_later_build_rolls_back_keeping_every_row() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let before = before(&s).await;
        let rock = s.id_at("T_Rock.png").await;
        s.engine.hooks().crash(Point::Indexed);
        let run = rebuild_killed(&s, &pack_id).await;
        assert_eq!(
            s.rows().await[&rock].0,
            in_pack("Art/T_Rock.png"),
            "carried"
        );
        assert!(
            s.count("SELECT COUNT(*) FROM import_moves WHERE kind = 'inserted'")
                .await
                > 0
        );

        reopen(&s).await;
        assert_eq!(status(&s, &run).await, "interrupted");
        restored(&s, &before).await;
    }

    #[tokio::test]
    async fn killed_after_the_swap_a_first_build_rolls_back_to_no_folder_and_no_rows() {
        let s = disk().await;
        s.engine.hooks().crash(Point::Indexed);
        let run = import_killed(&s).await;
        assert!(s.live().exists());

        reopen(&s).await;
        assert_eq!(status(&s, &run).await, "interrupted");
        nothing_built(&s).await;
    }

    /// The name-swap cases: `T_Rock.png` and `Readme.txt` trade names through two `rename`
    /// steps, so both rows are carried.
    async fn swap_names(s: &Setup) -> (String, Before) {
        let pack_id = s.import_golden().await;
        s.tag("T_Rock.png", "rock").await;
        s.tag("Readme.txt", "readme").await;
        let before = before(s).await;
        let (rock, readme) = (
            s.source_of("T_Rock.png").await,
            s.source_of("Readme.txt").await,
        );
        s.edit_steps(&pack_id, |steps| {
            for (source, name) in [
                (&rock, "Swap.tmp"),
                (&readme, "T_Rock.png"),
                (&rock, "Readme.txt"),
            ] {
                let step = PlaceStep::Rename {
                    source: source.clone(),
                    name: name.into(),
                };
                add(steps, step);
            }
        })
        .await;
        (pack_id, before)
    }

    #[tokio::test]
    async fn a_name_swap_killed_after_the_swap_or_between_the_carry_s_passes_rolls_back() {
        for point in [Point::Indexed, Point::CarryAside] {
            let s = disk().await;
            let (pack_id, before) = swap_names(&s).await;
            s.engine.hooks().crash(point);
            rebuild_killed(&s, &pack_id).await;
            assert_ne!(s.rows().await, before.rows, "{point:?}: rows moved");

            reopen(&s).await;
            assert_eq!(on_disk(&s.live()), before.tree, "{point:?}");
            assert_eq!(s.rows().await, before.rows, "{point:?}");
            s.settled().await;
            s.tree_untouched().await;
        }
    }

    #[tokio::test]
    async fn a_later_build_journaled_without_a_previous_folder_rolls_back_and_prunes_nothing() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let before = before(&s).await;
        let rock = s.id_at("T_Rock.png").await;
        s.engine.hooks().crash(Point::Indexed);
        rebuild_killed(&s, &pack_id).await;
        // As if the pack folder had been deleted outside OpenDAM and restored at drift: no live
        // folder was moved aside.
        std::fs::remove_dir_all(&beside(&s, "prev")[0]).unwrap();
        sqlx::query("UPDATE import_packs SET swap_prev = 0")
            .execute(&s.lib.pool)
            .await
            .unwrap();
        assert_eq!(
            s.rows().await[&rock].0,
            in_pack("Art/T_Rock.png"),
            "carried"
        );

        reopen(&s).await;
        assert!(!s.live().exists(), "the pack folder is gone again");
        assert_eq!(s.current_build().await, before.build);
        assert_eq!(
            s.rows().await,
            before.rows,
            "the carried row is back with its tag, the inserted rows are gone, and no index \
             pruned the previous build's rows"
        );
        s.settled().await;

        refresh(&s).await;
        assert_eq!(
            s.count(&format!(
                "SELECT COUNT(*) FROM assets WHERE file_path LIKE '{TARGET}/%'"
            ))
            .await,
            0,
            "a refresh prunes them, as for any deleted folder"
        );
        s.tree_untouched().await;
    }

    #[tokio::test]
    async fn killed_inside_a_rollback_recovery_finishes_it() {
        for point in [Point::RollbackAside, Point::RollbackRenamed] {
            let s = disk().await;
            let pack_id = later(&s).await;
            let before = before(&s).await;
            s.engine.hooks().fail(Point::Indexed);
            s.engine.hooks().crash(point);
            rebuild_killed(&s, &pack_id).await;
            assert_eq!(beside(&s, "failed").len(), 1, "{point:?}");
            assert_eq!(
                s.live().exists(),
                point == Point::RollbackRenamed,
                "{point:?}"
            );

            reopen(&s).await;
            restored(&s, &before).await;
        }

        let s = disk().await;
        s.engine.hooks().fail(Point::Indexed);
        s.engine.hooks().crash(Point::RollbackRenamed);
        import_killed(&s).await;
        assert_eq!(beside(&s, "failed").len(), 1);
        reopen(&s).await;
        nothing_built(&s).await;
    }

    /// Step 4's failure path with the staging folder deleted after the journal, so nothing of the
    /// build reaches the pack folder: killed after the journal (`Journaled`), or after the second
    /// rename failed and the previous folder (if any) went back (`Unjournal`).
    async fn staging_gone(s: &Setup, pause: &build::Pause) {
        reached(pause).await;
        std::fs::remove_dir_all(&beside(s, "import")[0]).unwrap();
        pause.release.notify_one();
    }

    #[tokio::test]
    async fn killed_in_step_4_s_failure_path_changes_nothing_on_disk() {
        for point in [Point::Journaled, Point::Unjournal] {
            let s = disk().await;
            let pack_id = later(&s).await;
            let before = before(&s).await;
            let pause = s.engine.hooks().pause(Point::Journaled);
            s.engine.hooks().crash(point);
            let run = s.rebuild(&pack_id).await;
            staging_gone(&s, &pause).await;
            killed(&s, &run.id).await;
            assert_eq!(s.pack().await.2.as_deref(), Some("swapping"), "{point:?}");

            reopen(&s).await;
            restored(&s, &before).await;
            not_blocked(&s, &pack_id).await;

            let s = disk().await;
            let pause = s.engine.hooks().pause(Point::Journaled);
            s.engine.hooks().crash(point);
            let run = s.engine.start_import(s.golden_request()).await.unwrap();
            staging_gone(&s, &pause).await;
            killed(&s, &run.id).await;
            assert_eq!(s.pack().await.2.as_deref(), Some("swapping"), "{point:?}");
            reopen(&s).await;
            nothing_built(&s).await;
            not_blocked(&s, &run.packs[0].pack_id).await;
        }
    }

    #[tokio::test]
    async fn a_state_no_step_leaves_is_left_as_found_and_blocks_the_pack() {
        let s = disk().await;
        let pack_id = later(&s).await;
        s.engine.hooks().crash(Point::Aside);
        let run = rebuild_killed(&s, &pack_id).await;
        let build = swap_build(&s).await;
        let failed = fs_swap::create_staging(&s.live(), "failed", &build).unwrap();
        let found = |s: &Setup| {
            (
                on_disk(&beside(s, "import")[0]),
                on_disk(&beside(s, "prev")[0]),
                s.live().exists(),
                failed.exists(),
            )
        };
        let (on_disk_before, rows) = (found(&s), s.rows().await);

        reopen(&s).await;
        assert_eq!(found(&s), on_disk_before, "nothing changes on disk");
        assert_eq!(s.rows().await, rows);
        assert_eq!(
            s.pack().await,
            (
                "failed".into(),
                s.current_build().await,
                Some("swapping".into())
            )
        );
        let ended = records::run(&s.lib.pool, "lib", &run).await.unwrap();
        assert_eq!(ended.status, "failed");
        assert!(
            ended.error.unwrap().contains("failed folder present"),
            "it names the state found"
        );
        assert!(
            s.engine.rebuild_pack("lib", &pack_id).await.is_err(),
            "its runs are blocked"
        );

        // A refresh recovers nothing either, and its index doesn't reach the pack.
        refresh(&s).await;
        assert_eq!(found(&s), on_disk_before);
        assert_eq!(s.rows().await, rows, "the pack's rows survive a refresh");
        assert_eq!(s.pack().await.2.as_deref(), Some("swapping"));
    }

    #[tokio::test]
    async fn a_recovery_that_fails_leaves_the_pack_to_no_index_and_a_later_one_finishes_it() {
        use std::os::unix::fs::PermissionsExt;
        let s = disk().await;
        let pack_id = later(&s).await;
        let before = before(&s).await;
        s.engine.hooks().crash(Point::Aside);
        rebuild_killed(&s, &pack_id).await;
        // Renaming `.prev` back fails while the folder holding the pack is read-only.
        let parent = s.live().parent().unwrap().to_path_buf();
        let mode = |m: u32| std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(m));
        mode(0o555).unwrap();
        if std::fs::create_dir(parent.join("probe")).is_ok() {
            mode(0o755).unwrap();
            eprintln!("Permissions don't stop this user (root?): skipped");
            return;
        }

        refresh(&s).await;
        let left = (s.rows().await, s.pack().await.2);
        mode(0o755).unwrap();
        assert_eq!(left.1.as_deref(), Some("swapping"), "recovery failed");
        assert_eq!(
            left.0, before.rows,
            "the refresh pruned none of the pack's rows, though its folder is aside"
        );

        refresh(&s).await;
        restored(&s, &before).await;
    }

    // ── When recovery runs ──────────────────────────────────────────────────

    #[tokio::test]
    async fn a_refresh_at_once_after_a_crash_mid_swap_finds_the_pack_recovered() {
        let pid = std::process::id();
        let started = watcher::process_start_time(pid).unwrap();
        let (Some(dead), Some(live)) = (
            watcher::token_naming(pid, started + 1),
            watcher::token_naming(pid, started),
        ) else {
            eprintln!("This machine's id is unknown, so no lock names a dead process: skipped");
            return;
        };
        let s = disk().await;
        let pack_id = later(&s).await;
        let before = before(&s).await;
        let old = s.id_at(OLD).await;
        s.engine.hooks().crash(Point::Indexed);
        rebuild_killed(&s, &pack_id).await;
        let lock = s.dir.path().join(".opendam/index.lock");

        // A live process's lock, or a fresh one from another machine, is respected.
        for token in [live, "another-machine 1 1 x".to_string()] {
            std::fs::write(&lock, &token).unwrap();
            reopen(&s).await;
            refresh(&s).await;
            assert_eq!(s.pack().await.2.as_deref(), Some("swapping"), "{token}");
            assert_eq!(s.rows().await[&old], before.rows[&old], "{token}");
        }

        // The killed app's lock, still fresh, names a process that is gone.
        std::fs::write(&lock, dead).unwrap();
        reopen(&s).await;
        refresh(&s).await;
        restored(&s, &before).await;
        assert_eq!(
            s.rows().await[&old],
            (in_pack(OLD), vec!["old".to_string()])
        );
    }

    #[tokio::test]
    async fn a_dropped_run_task_is_recovered_by_the_next_index_run_or_structure_pass() {
        for entry in ["index_paths", "rebuild_library"] {
            let s = disk().await;
            let pack_id = later(&s).await;
            let before = before(&s).await;
            s.engine.hooks().crash(Point::Indexed);
            let run = s.rebuild(&pack_id).await;
            // The task panicked during step 5 and let go of its guards without a rollback.
            s.wait_for(&run.id, "failed").await;
            assert_eq!(s.pack().await.2.as_deref(), Some("swapping"), "{entry}");

            match entry {
                "index_paths" => refresh(&s).await,
                _ => structure::rebuild_library(&s.engine.db, &s.lib).await,
            }
            restored(&s, &before).await;
        }
    }

    #[tokio::test]
    async fn a_library_that_opens_late_is_recovered_as_it_opens() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let before = before(&s).await;
        s.engine.hooks().crash(Point::Indexed);
        rebuild_killed(&s, &pack_id).await;
        s.engine.db.unregister_library("lib").await.unwrap();

        s.engine
            .db
            .register_library("lib", "lib", s.dir.path())
            .await
            .unwrap();
        restored(&s, &before).await;
    }

    #[tokio::test]
    async fn a_reconnect_during_the_swap_waits_for_the_build_and_runs_no_pass_meanwhile() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let journaled = s.engine.hooks().pause(Point::Journaled);
        let committed = s.engine.hooks().pause(Point::Committed);
        let run = s.rebuild(&pack_id).await;
        reached(&journaled).await;
        let passes = structure::passes_over(s.dir.path());
        let reconnect = tokio::spawn({
            let (lock, root) = (s.engine.db.index_lock("lib"), s.dir.path().to_path_buf());
            async move { db::open_library_db(&lock, &root).await.unwrap() }
        });
        journaled.release.notify_one();
        reached(&committed).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!reconnect.is_finished(), "the open waits on index_lock");
        assert_eq!(
            structure::passes_over(s.dir.path()),
            passes,
            "no pass ran in steps 3-6"
        );

        committed.release.notify_one();
        s.wait_for(&run.id, "done").await;
        reconnect.await.unwrap();
        assert_eq!(s.pack().await.2, None);
        s.settled().await;
        s.tree_untouched().await;
    }

    /// The pack is still mid-swap, and its previous-only row keeps its tag.
    async fn untouched(s: &Setup, old: &str) {
        assert_eq!(s.pack().await.2.as_deref(), Some("swapping"));
        assert_eq!(s.rows().await[old], (in_pack(OLD), vec!["old".to_string()]));
    }

    #[tokio::test]
    async fn a_second_machine_during_the_swap_yields_and_its_pass_is_late_but_not_lost() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let old = s.id_at(OLD).await;
        let pause = s.engine.hooks().pause(Point::Indexed);
        let run = s.rebuild(&pack_id).await;
        reached(&pause).await;
        let o = other(&s).await;

        watcher::index_paths(&o.db, "lib", &[root(&s)])
            .await
            .unwrap();
        untouched(&s, &old).await;
        structure::rebuild_library(&o.db, &o.lib).await;
        assert!(structure::is_pending(&s.lib.pool).await, "its pass is late");
        untouched(&s, &old).await;
        db::open_library_db(&o.db.index_lock("lib"), s.dir.path())
            .await
            .unwrap();
        untouched(&s, &old).await;

        pause.release.notify_one();
        s.wait_for(&run.id, "done").await;
        assert!(!structure::is_pending(&s.lib.pool).await, "the pass ran");
        s.settled().await;
        s.tree_untouched().await;
    }

    /// Makes run `run_id` another machine's, its heartbeat `age_ms` old.
    async fn owned_elsewhere(s: &Setup, run_id: String, age_ms: i64) {
        sqlx::query(
            "UPDATE import_runs SET owner_machine = 'another machine', owner_boot = 'b',
             heartbeat_at = ? WHERE id = ?",
        )
        .bind(chrono::Utc::now().timestamp_millis() - age_ms)
        .bind(run_id)
        .execute(&s.lib.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn another_machine_starting_during_staging_leaves_a_live_build_and_clears_a_dead_one() {
        let s = disk().await;
        let pack_id = later(&s).await;
        let o = other(&s).await;

        let pause = s.engine.hooks().pause(Point::Staged);
        let run = s.rebuild(&pack_id).await;
        reached(&pause).await;
        owned_elsewhere(&s, run.id.clone(), 0).await;
        let staged = (
            on_disk(&beside(&s, "import")[0]),
            s.count("SELECT COUNT(*) FROM import_outputs").await,
        );
        let engine = start_engine(&o.db);
        tokio::time::sleep(Duration::from_millis(200)).await;
        engine.abort();
        db::open_library_db(&o.db.index_lock("lib"), s.dir.path())
            .await
            .unwrap();
        assert_eq!(status(&s, &run.id).await, "running");
        assert_eq!(
            (
                on_disk(&beside(&s, "import")[0]),
                s.count("SELECT COUNT(*) FROM import_outputs").await,
            ),
            staged
        );
        pause.release.notify_one();
        s.wait_for(&run.id, "done").await;
        s.settled().await;

        let before = before(&s).await;
        let run = s.rebuild(&pack_id).await;
        reached(&pause).await;
        owned_elsewhere(&s, run.id.clone(), 11 * 60 * 1000).await;
        let engine = start_engine(&o.db);
        s.wait_for(&run.id, "interrupted").await;
        engine.abort();
        db::open_library_db(&o.db.index_lock("lib"), s.dir.path())
            .await
            .unwrap();
        assert!(beside(&s, "import").is_empty(), "its staging is deleted");
        assert_eq!(
            s.count(
                "SELECT COUNT(*) FROM import_outputs o JOIN import_packs p
                     ON p.id = o.pack_id WHERE o.build_id != p.current_build"
            )
            .await,
            0,
            "and its rows"
        );
        // The build, alive after all, fails at its re-check with nothing lost.
        pause.release.notify_one();
        s.wait_for(&run.id, "failed").await;
        assert_eq!(on_disk(&s.live()), before.tree);
        assert_eq!(s.rows().await, before.rows);
        assert_eq!(s.current_build().await, before.build);
        s.settled().await;
        s.tree_untouched().await;
    }
}
