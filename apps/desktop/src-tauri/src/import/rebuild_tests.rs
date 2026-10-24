//! Rebuilds (plan pack-import, Flow, A4 and A11's in-run cases): step edits, the carry of moved
//! files' rows, and rollbacks that keep every row, id and tag.
//!
//! The tests sit in a `mod tests` block, as in every other file, so source scans that stop there
//! (`structure::tests::metadata_writers`) read none of them.

mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};

    use serde_json::json;

    use crate::fs_swap;
    use crate::import::build::FINISHING;
    use crate::import::engine::tests::{setup, setup_with, Setup, TARGET};
    use crate::import::engine::*;
    use crate::import::unitypackage::tests::{golden, on_disk};

    const CHANGED: &str = "The pack changed during the build; Rebuild again";
    /// The nested package's `Art` folder, below the pack root once flattened.
    const HOUSES_ART: &str = "Houses/Built-In/Built-In_Pack/Pack/Art";

    /// The golden tree with `hoist Art` off: the extraction with its wrapper folders merged.
    fn unhoisted() -> Vec<String> {
        let mut files: Vec<String> = golden("synthetic.extract.txt")
            .into_iter()
            .map(|p| {
                p.strip_prefix("Built-In/Built-In_Pack/Pack/")
                    .map_or(p.clone(), str::to_string)
            })
            .collect();
        files.sort();
        files
    }

    fn in_pack(path: &str) -> String {
        format!("{TARGET}/{path}")
    }

    /// A row's id, path and tags, as a rebuild must keep them.
    type Rows = BTreeMap<String, (String, Vec<String>)>;

    impl Setup {
        /// Imports the golden pack (flatten, hoist Art) and returns its id.
        async fn import_golden(&self) -> String {
            let run = self
                .engine
                .start_import(self.golden_request())
                .await
                .unwrap();
            self.wait_for(&run.id, "done").await;
            run.packs[0].pack_id.clone()
        }

        /// Edits the pack's steps through `edit` and returns the new `recipe_rev`.
        async fn edit_steps(&self, pack_id: &str, edit: impl FnOnce(&mut Vec<Step>)) -> i64 {
            let mut steps = self.engine.pack_steps("lib", pack_id).await.unwrap().steps;
            edit(&mut steps);
            self.engine
                .set_pack_steps("lib", pack_id, steps)
                .await
                .unwrap()
        }

        async fn rebuild(&self, pack_id: &str) -> RunSummary {
            self.engine.rebuild_pack("lib", pack_id).await.unwrap()
        }

        async fn rebuild_until(&self, pack_id: &str, status: &str) -> RunSummary {
            let run = self.rebuild(pack_id).await;
            self.wait_for(&run.id, status).await
        }

        /// The asset id at `path` in the pack.
        async fn id_at(&self, path: &str) -> String {
            self.id_of(&in_pack(path)).await
        }

        /// The asset id at `path` in the library.
        async fn id_of(&self, path: &str) -> String {
            sqlx::query_scalar("SELECT id FROM assets WHERE file_path = ?")
                .bind(path)
                .fetch_one(&self.lib.pool)
                .await
                .unwrap_or_else(|e| panic!("no row at {path}: {e}"))
        }

        /// Tags the row at `path` in the pack with a tag of its own, named `name`, and returns its id.
        async fn tag(&self, path: &str, name: &str) -> String {
            self.tag_path(&in_pack(path), name).await
        }

        /// `tag`, for `path` in the library.
        async fn tag_path(&self, path: &str, name: &str) -> String {
            let id = self.id_of(path).await;
            for sql in [
                "INSERT OR IGNORE INTO tags (id, name) VALUES (?1, ?1)",
                "INSERT INTO asset_tags (asset_id, tag_id) SELECT ?2, ?1",
            ] {
                sqlx::query(sql)
                    .bind(name)
                    .bind(&id)
                    .execute(&self.lib.pool)
                    .await
                    .unwrap();
            }
            id
        }

        /// Every row of the library by id: its path and its tags.
        async fn rows(&self) -> Rows {
            let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
                "SELECT a.id, a.file_path, group_concat(t.tag_id) FROM assets a
                 LEFT JOIN (SELECT * FROM asset_tags ORDER BY tag_id) t ON t.asset_id = a.id
                 GROUP BY a.id",
            )
            .fetch_all(&self.lib.pool)
            .await
            .unwrap();
            rows.into_iter()
                .map(|(id, path, tags)| {
                    let tags =
                        tags.map_or(Vec::new(), |t| t.split(',').map(String::from).collect());
                    (id, (path, tags))
                })
                .collect()
        }

        /// The current build's file at `path` in the pack: its source path.
        async fn source_of(&self, path: &str) -> Vec<String> {
            let source: String = sqlx::query_scalar(
                "SELECT o.source_path FROM import_outputs o JOIN import_packs p ON p.id = o.pack_id
                 AND o.build_id = p.current_build WHERE o.path = ?",
            )
            .bind(in_pack(path))
            .fetch_one(&self.lib.pool)
            .await
            .unwrap();
            serde_json::from_str(&source).unwrap()
        }

        /// The current build's manifest as source path to path in the pack.
        async fn manifest_paths(&self) -> BTreeMap<String, String> {
            let rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT o.source_path, o.path FROM import_outputs o JOIN import_packs p
                 ON p.id = o.pack_id AND o.build_id = p.current_build",
            )
            .fetch_all(&self.lib.pool)
            .await
            .unwrap();
            rows.into_iter().collect()
        }

        /// The rows the pack's builds left nothing of: no journal, no other build's manifest rows,
        /// no row at a holding path, no folder beside the pack, and the pack not swapping.
        async fn settled(&self) {
            assert_eq!(self.count("SELECT COUNT(*) FROM import_moves").await, 0);
            assert_eq!(
                self.count(
                    "SELECT COUNT(*) FROM import_outputs o JOIN import_packs p ON p.id = o.pack_id
                     WHERE o.build_id != p.current_build"
                )
                .await,
                0
            );
            assert_eq!(
                self.count("SELECT COUNT(*) FROM assets WHERE file_path LIKE '%/.opendam-carry/%'")
                    .await,
                0
            );
            assert_eq!(
                self.count("SELECT COUNT(*) FROM import_packs WHERE swap_state IS NOT NULL")
                    .await,
                0
            );
            self.no_leftovers();
        }

        async fn current_build(&self) -> Option<String> {
            self.pack().await.1
        }

        /// `Other/Tree.png` keeps its id and tag.
        async fn tree_untouched(&self) {
            let rows = self.rows().await;
            assert_eq!(
                rows.get("tree"),
                Some(&("Other/Tree.png".to_string(), vec!["t1".to_string()]))
            );
        }
    }

    /// Waits for a build to reach `pause`, failing the test rather than hanging when it never does.
    async fn reached(pause: &build::Pause) {
        tokio::time::timeout(Duration::from_secs(30), pause.reached.notified())
            .await
            .expect("the build never reached its pause point");
    }

    /// Turns the step of `kind` on or off.
    fn toggle(steps: &mut [Step], kind: &str, enabled: bool) {
        steps
            .iter_mut()
            .find(|s| s.kind == kind)
            .unwrap_or_else(|| panic!("no {kind} step"))
            .enabled = enabled;
    }

    /// Inserts place `step` before the metadata step.
    fn add(steps: &mut Vec<Step>, step: PlaceStep) {
        let mut options = serde_json::to_value(&step).unwrap();
        let kind = options
            .as_object_mut()
            .unwrap()
            .remove("kind")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let at = steps.iter().position(|s| s.kind == "metadata").unwrap();
        steps.insert(
            at,
            Step {
                kind,
                enabled: true,
                options,
            },
        );
    }

    fn file_mtime(path: &Path) -> std::time::SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    #[tokio::test]
    async fn toggling_hoist_restores_each_tree_and_keeps_every_row_its_tags_and_its_preview() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let rock = s.tag("T_Rock.png", "rock").await;
        let readme = s.tag("Readme.txt", "readme").await;

        // The pack's files were written at another time than their entries' (as a generated file
        // is), and their previews are done.
        let written = UNIX_EPOCH + Duration::new(1_600_000_000, 123_456_789);
        for file in on_disk(&s.live()) {
            let path = s.live().join(file);
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(written)
                .unwrap();
        }
        sqlx::query("UPDATE import_outputs SET mtime_ns = 1600000000123456789")
            .execute(&s.lib.pool)
            .await
            .unwrap();
        crate::watcher::index_paths(
            &s.engine.db,
            "lib",
            &[s.dir.path().to_string_lossy().into()],
        )
        .await
        .unwrap();
        for sql in [
            "UPDATE assets SET preview_status = 'done', thumbnail_path = '.opendam/thumbs/' || id",
            "DELETE FROM preview_jobs",
        ] {
            sqlx::query(sql).execute(&s.lib.pool).await.unwrap();
        }
        let modified: Vec<(String, i64, String, Option<String>)> = sqlx::query_as(
            "SELECT id, modified_at, preview_status, thumbnail_path FROM assets ORDER BY id",
        )
        .fetch_all(&s.lib.pool)
        .await
        .unwrap();

        let mut rows = s.rows().await;
        for (on, tree) in [(false, unhoisted()), (true, golden("synthetic.place.txt"))] {
            let before = s.manifest_paths().await;
            let rev = s
                .edit_steps(&pack_id, |steps| toggle(steps, "hoist", on))
                .await;
            assert_eq!(rev, if on { 2 } else { 1 }, "each edit bumps recipe_rev");
            s.rebuild_until(&pack_id, "done").await;

            assert_eq!(on_disk(&s.live()), tree);
            let after = s.manifest_paths().await;
            // Every row moved with its file, by source path, with its id and tags.
            for (path, _) in rows.values_mut() {
                if let Some(source) = before.iter().find(|(_, p)| *p == path).map(|(s, _)| s) {
                    *path = after[source].clone();
                }
            }
            assert_eq!(s.rows().await, rows, "hoist {on}");
            s.settled().await;
            s.tree_untouched().await;
            assert_eq!(s.pack().await.0, "built");
            for file in &tree {
                assert_eq!(file_mtime(&s.live().join(file)), written, "{file}");
            }
            let now: Vec<(String, i64, String, Option<String>)> = sqlx::query_as(
                "SELECT id, modified_at, preview_status, thumbnail_path FROM assets ORDER BY id",
            )
            .fetch_all(&s.lib.pool)
            .await
            .unwrap();
            assert_eq!(now, modified, "no row changed, and no preview was queued");
            assert_eq!(s.count("SELECT COUNT(*) FROM preview_jobs").await, 0);
        }
        assert_eq!(s.id_at("T_Rock.png").await, rock);
        assert_eq!(s.id_at("Readme.txt").await, readme);
    }

    #[tokio::test]
    async fn renames_that_swap_two_names_keep_each_file_s_row_and_tags() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let rock = s.tag("T_Rock.png", "rock").await;
        let readme = s.tag("Readme.txt", "readme").await;
        let (rock_source, readme_source) = (
            s.source_of("T_Rock.png").await,
            s.source_of("Readme.txt").await,
        );
        let rock_sha = crate::export::hashes::sha256_file(&s.live().join("T_Rock.png")).unwrap();

        s.edit_steps(&pack_id, |steps| {
            for (source, name) in [
                (&rock_source, "Swap.tmp"),
                (&readme_source, "T_Rock.png"),
                (&rock_source, "Readme.txt"),
            ] {
                let step = PlaceStep::Rename {
                    source: source.clone(),
                    name: name.into(),
                };
                add(steps, step);
            }
        })
        .await;
        s.rebuild_until(&pack_id, "done").await;

        assert_eq!(
            crate::export::hashes::sha256_file(&s.live().join("Readme.txt")).unwrap(),
            rock_sha
        );
        let rows = s.rows().await;
        assert_eq!(
            rows[&rock],
            (in_pack("Readme.txt"), vec!["rock".to_string()])
        );
        assert_eq!(
            rows[&readme],
            (in_pack("T_Rock.png"), vec!["readme".to_string()])
        );
        let (ext, kind): (String, String) =
            sqlx::query_as("SELECT extension, kind FROM assets WHERE id = ?")
                .bind(&rock)
                .fetch_one(&s.lib.pool)
                .await
                .unwrap();
        assert_eq!((ext.as_str(), kind.as_str()), ("txt", "document"));
        s.settled().await;
        s.tree_untouched().await;
    }

    /// Build 2 of the displacement cases: the root's `T_Rock.png` (A) excluded, and the nested
    /// package's `T_Rock.png` (C) hoisted to the root, where A's row is.
    async fn displace(s: &Setup, pack_id: &str) {
        let rock = s.source_of("T_Rock.png").await;
        s.edit_steps(pack_id, |steps| {
            toggle(steps, "hoist", false);
            add(steps, PlaceStep::Exclude { source: rock });
            add(
                steps,
                PlaceStep::Hoist {
                    folder: HOUSES_ART.into(),
                },
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_carried_file_takes_a_displaced_previous_only_row_s_path_but_not_its_tags() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let a = s.tag("T_Rock.png", "a").await;
        let c = s.tag(&format!("{HOUSES_ART}/T_Rock.png"), "c").await;

        displace(&s, &pack_id).await;
        s.rebuild_until(&pack_id, "done").await;

        let rows = s.rows().await;
        assert_eq!(rows[&c], (in_pack("T_Rock.png"), vec!["c".to_string()]));
        assert!(!rows.contains_key(&a), "the excluded file's row is pruned");
        assert_eq!(
            s.count(
                "SELECT COUNT(*) FROM asset_tags t JOIN assets a ON a.id = t.asset_id
                 WHERE t.tag_id = 'a'"
            )
            .await,
            0
        );
        assert!(!s.live().join("Houses").exists());
        s.settled().await;
        s.tree_untouched().await;
    }

    /// The newly-included cases: the nested package's `T_Rock.png` (C) excluded and built, the
    /// root's `T_Rock.png` (A) tagged `a`, then an edit that turns C's exclude off, excludes A and
    /// hoists C to the root, where A's row is. C is new to that build, so no row is carried to it.
    /// Returns A's id.
    async fn include_over(s: &Setup, pack_id: &str) -> String {
        let c = s.source_of(&format!("{HOUSES_ART}/T_Rock.png")).await;
        s.edit_steps(pack_id, |steps| {
            add(steps, PlaceStep::Exclude { source: c })
        })
        .await;
        s.rebuild_until(pack_id, "done").await;
        let a = s.tag("T_Rock.png", "a").await;
        let rock = s.source_of("T_Rock.png").await;
        s.edit_steps(pack_id, |steps| {
            toggle(steps, "exclude", false);
            toggle(steps, "hoist", false);
            add(steps, PlaceStep::Exclude { source: rock });
            add(
                steps,
                PlaceStep::Hoist {
                    folder: HOUSES_ART.into(),
                },
            );
        })
        .await;
        a
    }

    #[tokio::test]
    async fn a_newly_included_file_takes_a_previous_only_row_s_path_but_not_its_row() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let a = include_over(&s, &pack_id).await;
        s.rebuild_until(&pack_id, "done").await;

        let rows = s.rows().await;
        assert!(!rows.contains_key(&a), "the excluded file's row is pruned");
        let at_root = s.id_at("T_Rock.png").await;
        assert_ne!(at_root, a);
        assert_eq!(
            rows[&at_root].1,
            Vec::<String>::new(),
            "a fresh row, no tags"
        );
        assert_eq!(
            s.count(
                "SELECT COUNT(*) FROM asset_tags t JOIN assets a ON a.id = t.asset_id
                 WHERE t.tag_id = 'a'"
            )
            .await,
            0
        );
        s.settled().await;
        s.tree_untouched().await;
    }

    #[tokio::test]
    async fn a_newly_included_file_then_a_failing_index_puts_the_displaced_row_back() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let a = include_over(&s, &pack_id).await;
        let (build, rows, tree) = (s.current_build().await, s.rows().await, on_disk(&s.live()));

        let pause = fail_at_index(&s).await;
        let run = s.rebuild(&pack_id).await;
        reached(&pause).await;
        assert_ne!(
            s.rows().await[&a].0,
            in_pack("T_Rock.png"),
            "moved aside before the index"
        );
        pause.release.notify_one();

        s.wait_for(&run.id, "failed").await;
        assert_eq!(on_disk(&s.live()), tree);
        assert_eq!(s.current_build().await, build);
        assert_eq!(s.rows().await, rows);
        assert_eq!(rows[&a], (in_pack("T_Rock.png"), vec!["a".to_string()]));
        s.settled().await;
        s.tree_untouched().await;
    }

    /// Pauses the next build at step 5 after its index, then fails it there, so it rolls back.
    async fn fail_at_index(s: &Setup) -> Arc<build::Pause> {
        let pause = s.engine.hooks().pause(Point::Indexed);
        s.engine.hooks().fail(Point::Indexed);
        pause
    }

    /// After a rollback: the golden tree and the build that made it are current again, with every
    /// row as `rows` had it, and nothing of the failed build is left.
    async fn rolled_back(s: &Setup, build: &Option<String>, rows: &Rows) {
        assert_eq!(on_disk(&s.live()), golden("synthetic.place.txt"));
        assert_eq!(&s.current_build().await, build);
        assert_eq!(s.pack().await.0, "built");
        assert_eq!(&s.rows().await, rows);
        s.settled().await;
        s.tree_untouched().await;
    }

    #[tokio::test]
    async fn a_failing_index_rolls_a_rebuild_back_keeping_moved_old_and_shared_rows() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        // Moved by the new build, only in the previous one, and in both.
        let rock = s.tag("T_Rock.png", "rock").await;
        s.tag("Textures/T_Log_D.png", "old").await;
        s.tag("Readme.txt", "readme").await;
        let (build, rows) = (s.current_build().await, s.rows().await);
        let old = s.source_of("Textures/T_Log_D.png").await;
        s.edit_steps(&pack_id, |steps| {
            toggle(steps, "hoist", false);
            add(steps, PlaceStep::Exclude { source: old });
        })
        .await;

        let pause = fail_at_index(&s).await;
        let run = s.rebuild(&pack_id).await;
        reached(&pause).await;
        assert_eq!(s.pack().await.2.as_deref(), Some("swapping"));
        assert_eq!(
            s.rows().await[&rock].0,
            in_pack("Art/T_Rock.png"),
            "carried"
        );
        assert!(
            s.count("SELECT COUNT(*) FROM import_moves WHERE kind = 'moved'")
                .await
                > 0
        );
        pause.release.notify_one();

        let failed = s.wait_for(&run.id, "failed").await;
        assert!(failed.error.unwrap().contains("injected failure"));
        rolled_back(&s, &build, &rows).await;
        assert_eq!(
            s.count(&format!(
                "SELECT COUNT(*) FROM assets WHERE file_path LIKE '{TARGET}/Art/%'"
            ))
            .await,
            0,
            "no row of a path only the failed build had"
        );
    }

    #[tokio::test]
    async fn a_name_swap_then_a_failing_index_puts_both_rows_back() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        s.tag("T_Rock.png", "rock").await;
        s.tag("Readme.txt", "readme").await;
        let (build, rows) = (s.current_build().await, s.rows().await);
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

        let pause = fail_at_index(&s).await;
        let run = s.rebuild(&pack_id).await;
        reached(&pause).await;
        let swapped = s.rows().await;
        assert_ne!(swapped, rows, "both rows were carried");
        pause.release.notify_one();

        s.wait_for(&run.id, "failed").await;
        rolled_back(&s, &build, &rows).await;
    }

    #[tokio::test]
    async fn a_displaced_row_then_a_failing_index_puts_both_rows_back() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        s.tag("T_Rock.png", "a").await;
        s.tag(&format!("{HOUSES_ART}/T_Rock.png"), "c").await;
        let (build, rows) = (s.current_build().await, s.rows().await);
        displace(&s, &pack_id).await;

        s.engine.hooks().fail(Point::Indexed);
        s.rebuild_until(&pack_id, "failed").await;
        rolled_back(&s, &build, &rows).await;
    }

    #[tokio::test]
    async fn a_rebuild_whose_staging_changed_before_the_swap_fails_and_changes_nothing() {
        for whole in [false, true] {
            let s = setup().await;
            let pack_id = s.import_golden().await;
            s.tag("T_Rock.png", "rock").await;
            let (build, rows) = (s.current_build().await, s.rows().await);
            s.edit_steps(&pack_id, |steps| toggle(steps, "hoist", false))
                .await;

            let pause = s.engine.hooks().pause(Point::Staged);
            let run = s.rebuild(&pack_id).await;
            reached(&pause).await;
            let staging = fs_swap::leftovers(&s.live(), "import").unwrap().remove(0);
            if whole {
                std::fs::remove_dir_all(&staging).unwrap();
            } else {
                std::fs::remove_file(staging.join("Art/T_Rock.png")).unwrap();
            }
            pause.release.notify_one();

            let failed = s.wait_for(&run.id, "failed").await;
            assert_eq!(failed.error.as_deref(), Some(CHANGED), "whole: {whole}");
            rolled_back(&s, &build, &rows).await;
        }
    }

    #[tokio::test]
    async fn a_failing_second_rename_puts_the_previous_folder_back() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        s.tag("T_Rock.png", "rock").await;
        let (build, rows) = (s.current_build().await, s.rows().await);
        s.edit_steps(&pack_id, |steps| toggle(steps, "hoist", false))
            .await;

        s.engine.hooks().fail(Point::MoveIn);
        let failed = s.rebuild_until(&pack_id, "failed").await;
        assert!(failed
            .error
            .unwrap()
            .contains("Could not move the built pack"));
        rolled_back(&s, &build, &rows).await;
    }

    #[tokio::test]
    async fn a_file_or_step_changed_during_a_rebuild_fails_it_at_the_recheck() {
        for change in ["file", "step"] {
            let s = setup().await;
            let pack_id = s.import_golden().await;
            let (build, rows) = (s.current_build().await, s.rows().await);
            s.edit_steps(&pack_id, |steps| toggle(steps, "hoist", false))
                .await;

            let pause = s.engine.hooks().pause(Point::Staged);
            let run = s.rebuild(&pack_id).await;
            reached(&pause).await;
            match change {
                "file" => std::fs::write(s.live().join("Readme.txt"), "edited outside").unwrap(),
                // As another machine could: edits here are refused during a run.
                _ => {
                    sqlx::query("UPDATE import_packs SET recipe_rev = recipe_rev + 1")
                        .execute(&s.lib.pool)
                        .await
                        .unwrap();
                }
            }
            pause.release.notify_one();

            let failed = s.wait_for(&run.id, "failed").await;
            assert_eq!(failed.error.as_deref(), Some(CHANGED), "{change}");
            assert_eq!(s.current_build().await, build);
            assert_eq!(s.rows().await, rows);
            s.settled().await;
            match change {
                "file" => assert_eq!(
                    std::fs::read_to_string(s.live().join("Readme.txt")).unwrap(),
                    "edited outside",
                    "the live folder is unchanged"
                ),
                _ => assert_eq!(on_disk(&s.live()), golden("synthetic.place.txt")),
            }
        }
    }

    #[tokio::test]
    async fn a_pack_folder_changed_outside_opendam_fails_the_rebuild_before_it_stages() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        std::fs::write(s.live().join("Added.txt"), "mine").unwrap();
        let build = s.current_build().await;

        let failed = s.rebuild_until(&pack_id, "failed").await;
        assert_eq!(
            failed.error.as_deref(),
            Some(
                format!(
                    "{} changed outside OpenDAM since the pack's last build",
                    in_pack("Added.txt")
                )
                .as_str()
            )
        );
        assert_eq!(s.current_build().await, build);
        assert!(s.live().join("Added.txt").exists());
        s.settled().await;
    }

    #[tokio::test]
    async fn cancel_during_step_5_finishes_the_rebuild_by_committing_or_rolling_back() {
        for fails in [false, true] {
            let s = setup().await;
            let pack_id = s.import_golden().await;
            let old_id = s.tag("Textures/T_Log_D.png", "old").await;
            let (build, rows) = (s.current_build().await, s.rows().await);
            let old = s.source_of("Textures/T_Log_D.png").await;
            s.edit_steps(&pack_id, |steps| {
                add(steps, PlaceStep::Exclude { source: old });
            })
            .await;

            let pause = s.engine.hooks().pause(Point::Indexed);
            if fails {
                s.engine.hooks().fail(Point::Indexed);
            }
            let run = s.rebuild(&pack_id).await;
            reached(&pause).await;
            assert_eq!(
                s.engine.cancel_run("lib", &run.id).await,
                Err(FINISHING.into())
            );
            pause.release.notify_one();

            if fails {
                s.wait_for(&run.id, "failed").await;
                rolled_back(&s, &build, &rows).await;
                assert_eq!(
                    s.rows().await[&old_id],
                    (in_pack("Textures/T_Log_D.png"), vec!["old".to_string()])
                );
            } else {
                s.wait_for(&run.id, "done").await;
                assert!(!s.live().join("Textures/T_Log_D.png").exists());
                assert!(
                    !s.rows().await.contains_key(&old_id),
                    "committed and pruned"
                );
                assert_ne!(s.current_build().await, build);
                s.settled().await;
            }
        }
    }

    #[tokio::test]
    async fn cancel_before_the_lock_leaves_the_built_pack_and_its_rows_alone() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        s.tag("T_Rock.png", "rock").await;
        let (build, rows) = (s.current_build().await, s.rows().await);
        s.edit_steps(&pack_id, |steps| toggle(steps, "hoist", false))
            .await;

        let pause = s.engine.hooks().pause(Point::Staged);
        let run = s.rebuild(&pack_id).await;
        reached(&pause).await;
        s.engine.cancel_run("lib", &run.id).await.unwrap();
        pause.release.notify_one();

        s.wait_for(&run.id, "cancelled").await;
        rolled_back(&s, &build, &rows).await;
    }

    #[tokio::test]
    async fn rows_outside_the_pack_survive_a_rebuild_s_commit_and_its_rollback() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        // A second pack whose target shares the first one's name as a prefix.
        let mut beside = s.golden_request();
        beside.target = format!("{TARGET}Plus");
        let run = s.engine.start_import(beside).await.unwrap();
        s.wait_for(&run.id, "done").await;
        let plus = s.dir.path().join(format!("{TARGET}Plus"));
        let tagged = s
            .tag_path(&format!("{TARGET}Plus/T_Rock.png"), "plus")
            .await;
        let plus_rows = |rows: &Rows| -> Rows {
            rows.iter()
                .filter(|(_, (p, _))| {
                    p.starts_with(&format!("{TARGET}Plus/")) || p.starts_with("Other/")
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };
        let outside = plus_rows(&s.rows().await);
        assert_eq!(
            outside[&tagged].0,
            format!("{TARGET}Plus/T_Rock.png"),
            "{tagged}"
        );

        s.edit_steps(&pack_id, |steps| toggle(steps, "hoist", false))
            .await;
        s.rebuild_until(&pack_id, "done").await;
        assert_eq!(on_disk(&s.live()), unhoisted());
        assert_eq!(plus_rows(&s.rows().await), outside);

        s.edit_steps(&pack_id, |steps| toggle(steps, "hoist", true))
            .await;
        s.engine.hooks().fail(Point::Indexed);
        s.rebuild_until(&pack_id, "failed").await;
        assert_eq!(on_disk(&s.live()), unhoisted());
        assert_eq!(plus_rows(&s.rows().await), outside);
        assert_eq!(on_disk(&plus), golden("synthetic.place.txt"));
    }

    #[tokio::test]
    async fn step_edits_and_rebuilds_are_refused_while_a_run_for_the_pack_is_queued_or_running() {
        let s = setup_with(false).await;
        let run = s.engine.start_import(s.golden_request()).await.unwrap();
        let pack_id = run.packs[0].pack_id.clone();
        let steps = s.engine.pack_steps("lib", &pack_id).await.unwrap();
        assert_eq!(steps.recipe_rev, 0);
        let refused = Err(BUSY.to_string());

        // Queued.
        assert_eq!(
            s.engine
                .set_pack_steps("lib", &pack_id, steps.steps.clone())
                .await
                .map(|_| ()),
            refused
        );
        assert_eq!(
            s.engine.rebuild_pack("lib", &pack_id).await.map(|_| ()),
            refused
        );

        // Running.
        let pause = s.engine.hooks().pause(Point::Staged);
        tokio::spawn(s.engine.clone().start());
        reached(&pause).await;
        assert_eq!(
            s.engine
                .set_pack_steps("lib", &pack_id, steps.steps.clone())
                .await
                .map(|_| ()),
            refused
        );
        assert_eq!(
            s.engine.rebuild_pack("lib", &pack_id).await.map(|_| ()),
            refused
        );
        assert_eq!(
            s.engine.pack_steps("lib", &pack_id).await.unwrap(),
            steps,
            "nothing was recorded"
        );
        pause.release.notify_one();
        s.wait_for(&run.id, "done").await;

        // Once the run is over: an edit bumps recipe_rev, and a queued rebuild refuses the next.
        let mut edited = steps.steps.clone();
        toggle(&mut edited, "hoist", false);
        assert_eq!(
            s.engine
                .set_pack_steps("lib", &pack_id, edited.clone())
                .await,
            Ok(1)
        );
        let rebuild = s.engine.rebuild_pack("lib", &pack_id).await.unwrap();
        assert_eq!(rebuild.packs[0].recipe_rev, 1);
        assert_eq!(rebuild.kind, "rebuild");
        reached(&pause).await;
        assert_eq!(
            s.engine.rebuild_pack("lib", &pack_id).await.map(|_| ()),
            refused
        );
        assert_eq!(
            s.engine
                .set_pack_steps("lib", &pack_id, edited)
                .await
                .map(|_| ()),
            refused
        );
        pause.release.notify_one();
        s.wait_for(&rebuild.id, "done").await;
    }

    #[tokio::test]
    async fn step_edits_a_build_couldn_t_run_are_refused() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let steps = s.engine.pack_steps("lib", &pack_id).await.unwrap().steps;

        let mut no_extract = steps.clone();
        toggle(&mut no_extract, "extract_unitypackage", false);
        let mut bad_options = steps.clone();
        bad_options.push(Step {
            kind: "hoist".into(),
            enabled: false,
            options: json!({ "folders": "Art" }),
        });
        let mut unknown = steps.clone();
        unknown.push(Step {
            kind: "unreal".into(),
            enabled: false,
            options: json!({}),
        });
        for edit in [no_extract, bad_options, unknown] {
            assert!(s
                .engine
                .set_pack_steps("lib", &pack_id, edit)
                .await
                .is_err());
        }
        assert_eq!(
            s.engine.set_pack_steps("lib", "nope", steps).await,
            Err("The pack isn't an imported pack of this library".into())
        );
        assert_eq!(
            s.engine
                .pack_steps("lib", &pack_id)
                .await
                .unwrap()
                .recipe_rev,
            0
        );
    }
}
