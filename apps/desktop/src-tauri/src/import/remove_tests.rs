//! Remove import (plan pack-import, Remove import, R38, A11 and A13): a Remove deletes exactly
//! the manifest's files and the folders they leave empty, and the pack's records; one whose
//! re-check finds drift deletes nothing; and one killed at an injected point (a panic, which
//! leaves the disk and the records as a killed process would) is finished by recovery.
//!
//! The tests sit in a `mod tests` block, as in every other file, so source scans that stop there
//! (`structure::tests::metadata_writers`) read none of them.

mod tests {
    use std::collections::BTreeSet;

    use super::super::rebuild_tests::tests::{in_pack, Rows};
    use super::super::recovery_tests::tests::{disk, killed, refresh, reopen, status};
    use crate::import::drift::{self, Change};
    use crate::import::engine::tests::{setup, Setup, TARGET};
    use crate::import::engine::*;
    use crate::import::remove::RemovePreview;
    use crate::import::unitypackage::tests::on_disk;

    /// The second pack's target, which shares the first one's name as a prefix.
    fn plus() -> String {
        format!("{TARGET}Plus")
    }

    impl Setup {
        async fn remove_preview(&self, pack_id: &str) -> RemovePreview {
            self.engine.remove_preview("lib", pack_id).await.unwrap()
        }

        /// Queues the pack's Remove with the outside changes its preview shows now.
        async fn remove(&self, pack_id: &str) -> RunSummary {
            let seen: Vec<drift::Seen> = self
                .remove_preview(pack_id)
                .await
                .drift
                .iter()
                .map(drift::Seen::from)
                .collect();
            self.engine.remove_pack("lib", pack_id, seen).await.unwrap()
        }

        /// A second import of the golden pack at `plus()`, recorded under the first pack's
        /// import, with a tagged row. Returns its pack id.
        async fn second_pack(&self, first: &str) -> String {
            let mut request = self.golden_request();
            request.target = plus();
            let run = self.engine.start_import(request).await.unwrap();
            self.wait_for(&run.id, "done").await;
            let pack_id = run.packs[0].pack_id.clone();
            // Imports have one pack each so far: this one joins the first one's import, so its
            // row stays until the last of the two is removed.
            let own: String = sqlx::query_scalar("SELECT import_id FROM import_packs WHERE id = ?")
                .bind(&pack_id)
                .fetch_one(&self.lib.pool)
                .await
                .unwrap();
            for sql in [
                "UPDATE import_packs SET import_id = (SELECT import_id FROM import_packs
                 WHERE id = ?1) WHERE id = ?2",
                "DELETE FROM imports WHERE id = ?3",
            ] {
                sqlx::query(sql)
                    .bind(first)
                    .bind(&pack_id)
                    .bind(&own)
                    .execute(&self.lib.pool)
                    .await
                    .unwrap();
            }
            self.tag_path(&format!("{}/T_Rock.png", plus()), "plus")
                .await;
            pack_id
        }

        /// The pack's current manifest paths.
        async fn manifest_of(&self, pack_id: &str) -> Vec<String> {
            sqlx::query_scalar(
                "SELECT o.path FROM import_outputs o JOIN import_packs p ON p.id = o.pack_id
                 AND o.build_id = p.current_build WHERE p.id = ? ORDER BY o.path",
            )
            .bind(pack_id)
            .fetch_all(&self.lib.pool)
            .await
            .unwrap()
        }

        /// How many of each import table's rows name pack `pack_id`.
        async fn records_of(&self, pack_id: &str) -> i64 {
            let mut count = 0;
            for table in ["import_outputs", "import_moves", "import_steps"] {
                count += self
                    .count(&format!(
                        "SELECT COUNT(*) FROM {table} WHERE pack_id = '{pack_id}'"
                    ))
                    .await;
            }
            count
                + self
                    .count(&format!(
                        "SELECT COUNT(*) FROM import_packs WHERE id = '{pack_id}'"
                    ))
                    .await
        }

        /// Every row outside the first pack, with its id, path and tags.
        async fn rows_outside(&self) -> Rows {
            let prefix = format!("{TARGET}/");
            self.rows()
                .await
                .into_iter()
                .filter(|(_, (path, _))| !path.starts_with(&prefix))
                .collect()
        }

        /// The rows below the first pack's folder, by path.
        async fn rows_inside(&self) -> BTreeSet<String> {
            let prefix = format!("{TARGET}/");
            self.rows()
                .await
                .into_values()
                .map(|(path, _)| path)
                .filter(|path| path.starts_with(&prefix))
                .collect()
        }
    }

    #[tokio::test]
    async fn a_remove_deletes_exactly_the_manifest_s_files_and_folders_then_its_records() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let second = s.second_pack(&pack_id).await;
        s.tag("T_Rock.png", "rock").await;
        let outside = s.rows_outside().await;
        assert!(outside.contains_key("tree") && outside.len() > 1);
        // A changed file, whose bytes the Remove loses, and an added one, which it leaves.
        std::fs::write(s.live().join("Readme.txt"), "yours").unwrap();
        std::fs::create_dir_all(s.live().join("Notes")).unwrap();
        std::fs::write(s.live().join("Notes/Yours.png"), "png").unwrap();
        crate::watcher::index_paths(&s.engine.db, "lib", &[s.live().to_string_lossy().into()])
            .await
            .unwrap();

        let preview = s.remove_preview(&pack_id).await;
        let files: Vec<String> = preview.files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(files, s.manifest_of(&pack_id).await);
        assert!(files.contains(&in_pack("T_Rock.png.meta")) && files.len() > 10);
        let drift: Vec<(String, Change)> = preview
            .drift
            .iter()
            .map(|d| (d.path.clone(), d.change))
            .collect();
        assert_eq!(
            drift,
            [
                (in_pack("Notes/Yours.png"), Change::Added),
                (in_pack("Readme.txt"), Change::Changed),
            ]
        );

        let run = s.remove(&pack_id).await;
        assert_eq!(run.kind, "remove");
        s.wait_for(&run.id, "done").await;
        assert_eq!(
            on_disk(&s.live()),
            ["Notes/Yours.png"],
            "only the added file stays"
        );
        assert_eq!(
            s.rows_inside().await,
            BTreeSet::from([in_pack("Notes/Yours.png")])
        );
        assert_eq!(
            s.rows_outside().await,
            outside,
            "rows outside keep their tags"
        );
        assert_eq!(s.records_of(&pack_id).await, 0);
        assert_eq!(
            s.count("SELECT COUNT(*) FROM imports").await,
            1,
            "the import keeps its row while it has a pack"
        );
        s.no_leftovers();
        assert!(s
            .events
            .lock()
            .unwrap()
            .contains(&ImportEvent::LibraryUpdated));

        // The import's last pack: its folder goes, with the import's row.
        let plus_folder = s.dir.path().join(plus());
        assert!(plus_folder.exists());
        let run = s.remove(&second).await;
        s.wait_for(&run.id, "done").await;
        assert!(!plus_folder.exists(), "the folder it left empty goes");
        assert!(s.dir.path().join("Pub").exists(), "never a folder above it");
        assert_eq!(s.records_of(&second).await, 0);
        assert_eq!(s.count("SELECT COUNT(*) FROM imports").await, 0);
        assert_eq!(
            s.count(&format!(
                "SELECT COUNT(*) FROM assets WHERE file_path LIKE '{}/%'",
                plus()
            ))
            .await,
            0
        );
        s.tree_untouched().await;
        assert_eq!(on_disk(&s.live()), ["Notes/Yours.png"]);
    }

    #[tokio::test]
    async fn a_remove_whose_re_check_finds_drift_fails_before_the_journal_deleting_nothing() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let tree = on_disk(&s.live());
        let (rows, manifest) = (s.rows().await, s.manifest_of(&pack_id).await);

        // Holding the library's index lock keeps the Remove at step 1.
        let index_lock = s.engine.db.index_lock("lib");
        let guard = index_lock.lock().await;
        s.events.lock().unwrap().clear();
        let run = s.remove(&pack_id).await;
        s.wait_for_step("waiting for the library").await;
        std::fs::write(s.live().join("Readme.txt"), "changed meanwhile").unwrap();
        drop(guard);

        let run = s.wait_for(&run.id, "failed").await;
        let error = run.error.unwrap();
        assert!(
            error.starts_with(&format!(
                "{} changed outside OpenDAM since the Remove was asked for",
                in_pack("Readme.txt")
            )),
            "{error}"
        );
        assert_eq!(s.pack().await.2, None, "never journaled");
        assert_eq!(on_disk(&s.live()), tree);
        assert_eq!(s.rows().await, rows);
        assert_eq!(s.manifest_of(&pack_id).await, manifest);
        assert_eq!(s.count("SELECT COUNT(*) FROM imports").await, 1);
        assert_eq!(
            s.pack().await.0,
            "built",
            "a failed Remove leaves the pack built"
        );

        // Queued now, a Remove must show the change first.
        let refused = s.engine.remove_pack("lib", &pack_id, Vec::new()).await;
        assert_eq!(refused.unwrap_err(), drift::STALE);
    }

    /// A Remove of the golden pack, beside a second pack and `Other/Tree.png`, killed at
    /// `point`. Returns the pack ids, the run's id, and the rows outside the pack before it.
    async fn removed_killed(s: &Setup, point: Point) -> (String, String, String, Rows) {
        let pack_id = s.import_golden().await;
        let second = s.second_pack(&pack_id).await;
        s.tag("T_Rock.png", "rock").await;
        let outside = s.rows_outside().await;
        s.engine.hooks().crash(point);
        let run = s.remove(&pack_id).await;
        killed(s, &run.id).await;
        assert_eq!(s.pack_state(&pack_id).await.as_deref(), Some("removing"));
        // While journaled, nothing else may touch the pack.
        let steps = s.engine.pack_steps("lib", &pack_id).await.unwrap().steps;
        let refused = s.engine.set_pack_steps("lib", &pack_id, steps, None).await;
        assert_eq!(refused.unwrap_err(), BUSY);
        let preview = s.engine.remove_preview("lib", &pack_id).await;
        assert_eq!(
            preview.unwrap_err(),
            BUSY,
            "no confirmation of a pack mid-Remove"
        );
        assert_eq!(
            s.engine
                .remove_pack("lib", &pack_id, Vec::new())
                .await
                .unwrap_err(),
            BUSY
        );
        (pack_id, second, run.id, outside)
    }

    impl Setup {
        async fn pack_state(&self, pack_id: &str) -> Option<String> {
            sqlx::query_scalar("SELECT swap_state FROM import_packs WHERE id = ?")
                .bind(pack_id)
                .fetch_one(&self.lib.pool)
                .await
                .unwrap()
        }
    }

    /// Recovery finished the Remove: every manifest file and its row, and the pack's records,
    /// are gone, the run reads `interrupted`, and nothing outside the pack changed.
    async fn finished(s: &Setup, pack_id: &str, second: &str, run_id: &str, outside: &Rows) {
        assert!(
            !s.live().exists(),
            "every file and folder of the pack is gone"
        );
        assert!(s.rows_inside().await.is_empty());
        assert_eq!(s.records_of(pack_id).await, 0);
        assert_eq!(s.rows_outside().await, *outside);
        assert!(
            !s.manifest_of(second).await.is_empty(),
            "the other pack is untouched"
        );
        assert_eq!(
            s.count("SELECT COUNT(*) FROM imports").await,
            1,
            "the import keeps its row for its other pack"
        );
        assert_eq!(status(s, run_id).await, "interrupted");
        s.no_leftovers();
        s.tree_untouched().await;
    }

    #[tokio::test]
    async fn killed_mid_delete_the_next_open_finishes_the_remove_with_no_re_check() {
        let s = disk().await;
        let (pack_id, second, run_id, outside) = removed_killed(&s, Point::RemoveDeleting).await;
        let manifest = s.manifest_of(&pack_id).await;
        let left = manifest
            .iter()
            .filter(|path| s.dir.path().join(path).exists())
            .count();
        assert!(
            left > 0 && left < manifest.len(),
            "killed midway: {left} of {} files left",
            manifest.len()
        );
        assert!(!s.rows_inside().await.is_empty(), "nothing pruned yet");

        reopen(&s).await;
        finished(&s, &pack_id, &second, &run_id, &outside).await;
    }

    #[tokio::test]
    async fn killed_after_the_prune_the_next_refresh_finishes_the_remove() {
        let s = disk().await;
        let (pack_id, second, run_id, outside) = removed_killed(&s, Point::RemovePruned).await;
        assert!(!s.live().exists());
        assert!(s.rows_inside().await.is_empty(), "pruned");
        assert!(
            s.records_of(&pack_id).await > 0,
            "the records are still there"
        );

        refresh(&s).await;
        finished(&s, &pack_id, &second, &run_id, &outside).await;
    }
}
