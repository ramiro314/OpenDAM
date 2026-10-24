//! Outside changes (plan pack-import, Edits after an import, A4's and A6's kept cases): the
//! drift check before a Rebuild, the choices each file's origin offers, and what each does.
//!
//! The tests sit in a `mod tests` block, as in every other file, so source scans that stop there
//! (`structure::tests::metadata_writers`) read none of them.

mod tests {
    use std::collections::BTreeSet;

    use super::super::rebuild_tests::tests::{add, in_pack, reached, toggle};
    use crate::import::drift::{Change, Choice, Difference, Kind, STALE};
    use crate::import::engine::tests::{setup, Setup};
    use crate::import::engine::*;
    use crate::import::metadata;

    const CHANGED: &str = "The pack changed during the build; Rebuild again";

    /// A difference as a test compares it: the path in the pack, what changed, the file's kind
    /// and the choices offered.
    type Seen = (String, Change, Kind, Vec<Choice>);

    impl Setup {
        async fn drift(&self, pack_id: &str) -> Vec<Difference> {
            self.engine.check_drift("lib", pack_id).await.unwrap()
        }

        async fn seen(&self, pack_id: &str) -> Vec<Seen> {
            let prefix = in_pack("");
            self.drift(pack_id)
                .await
                .into_iter()
                .map(|d| {
                    let rel = d.path.strip_prefix(&prefix).unwrap().to_string();
                    (rel, d.change, d.kind, d.choices)
                })
                .collect()
        }

        /// Indexes the whole library, as a refresh does.
        async fn refresh(&self) {
            crate::watcher::index_paths(
                &self.engine.db,
                "lib",
                &[self.dir.path().to_string_lossy().into_owned()],
            )
            .await
            .unwrap();
        }

        fn write(&self, path: &str, text: &str) {
            std::fs::write(self.live().join(path), text).unwrap();
        }

        fn read(&self, path: &str) -> String {
            std::fs::read_to_string(self.live().join(path)).unwrap()
        }

        /// The current build's file at `path` in the pack: its origin and source path.
        async fn origin_of(&self, path: &str) -> (String, Option<String>) {
            sqlx::query_as(
                "SELECT o.origin, o.source_path FROM import_outputs o JOIN import_packs p
                 ON p.id = o.pack_id AND o.build_id = p.current_build WHERE o.path = ?",
            )
            .bind(in_pack(path))
            .fetch_one(&self.lib.pool)
            .await
            .unwrap()
        }

        async fn kept(&self, pack_id: &str) -> Vec<String> {
            self.engine.pack_steps("lib", pack_id).await.unwrap().kept
        }

        async fn metadata_on(&self, pack_id: &str) -> bool {
            let steps = self.engine.pack_steps("lib", pack_id).await.unwrap().steps;
            steps.iter().find(|s| s.kind == "metadata").unwrap().enabled
        }

        async fn ids(&self) -> BTreeSet<String> {
            self.rows().await.into_keys().collect()
        }
    }

    fn first(d: &Difference) -> Choice {
        d.choices[0]
    }

    fn last(d: &Difference) -> Choice {
        *d.choices.last().unwrap()
    }

    /// The golden pack with a file added (`Notes.txt`, indexed and tagged), one changed
    /// (`Readme.txt`, tagged) and one deleted (`Textures/T_Log_D.png`) outside OpenDAM.
    async fn drifted(s: &Setup) -> (String, String, String) {
        let pack_id = s.import_golden().await;
        s.write("Notes.txt", "mine");
        s.write("Readme.txt", "edited outside");
        std::fs::remove_file(s.live().join("Textures/T_Log_D.png")).unwrap();
        s.refresh().await;
        let notes = s.tag("Notes.txt", "notes").await;
        let readme = s.tag("Readme.txt", "readme").await;
        (pack_id, notes, readme)
    }

    /// A6: an added, a changed and a missing file are listed, each with its choices; keep,
    /// keep yours and record as `exclude` carry the added and changed files as `kept`, with
    /// their rows and tags, and append the `exclude`. Later rebuilds carry the kept files too.
    #[tokio::test]
    async fn keep_keep_yours_and_exclude_carry_the_files_and_record_the_step() {
        let s = setup().await;
        let (pack_id, notes, readme) = drifted(&s).await;
        let log = s.source_of("Textures/T_Log_D.png.meta").await;
        let mut log_source = log.clone();
        let last = log_source.last_mut().unwrap();
        *last = last.strip_suffix(".meta").unwrap().to_string();
        assert_eq!(
            s.seen(&pack_id).await,
            [
                (
                    "Notes.txt".into(),
                    Change::Added,
                    Kind::Outside,
                    vec![Choice::Keep, Choice::Discard]
                ),
                (
                    "Readme.txt".into(),
                    Change::Changed,
                    Kind::Extracted,
                    vec![Choice::Keep, Choice::Recipe]
                ),
                (
                    "Textures/T_Log_D.png".into(),
                    Change::Missing,
                    Kind::Extracted,
                    vec![Choice::Exclude, Choice::Restore]
                ),
            ]
        );
        let rev = s
            .engine
            .pack_steps("lib", &pack_id)
            .await
            .unwrap()
            .recipe_rev;

        let run = s.rebuild_choosing(&pack_id, first).await;
        s.wait_for(&run.id, "done").await;
        assert_eq!(s.read("Notes.txt"), "mine");
        assert_eq!(s.read("Readme.txt"), "edited outside");
        assert!(!s.live().join("Textures/T_Log_D.png").exists());
        assert!(
            !s.live().join("Textures/T_Log_D.png.meta").exists(),
            "its step drops its .meta"
        );
        assert_eq!(s.origin_of("Notes.txt").await, ("kept".into(), None));
        let readme_source = serde_json::to_string(&s.source_of("Readme.txt").await).unwrap();
        assert_eq!(
            s.origin_of("Readme.txt").await,
            ("kept".into(), Some(readme_source)),
            "a kept copy keeps the replaced file's source path"
        );
        let steps = s.engine.pack_steps("lib", &pack_id).await.unwrap();
        assert_eq!(steps.recipe_rev, rev + 1, "the run's recipe edit");
        assert_eq!(
            steps.steps.last().unwrap(),
            &Step {
                kind: "exclude".into(),
                enabled: true,
                options: serde_json::json!({ "source": log_source }),
            }
        );
        assert_eq!(steps.kept, [in_pack("Notes.txt"), in_pack("Readme.txt")]);
        let rows = s.rows().await;
        assert_eq!(rows[&notes], (in_pack("Notes.txt"), vec!["notes".into()]));
        assert_eq!(
            rows[&readme],
            (in_pack("Readme.txt"), vec!["readme".into()])
        );
        assert!(s.drift(&pack_id).await.is_empty(), "nothing differs now");
        s.settled().await;
        s.tree_untouched().await;

        // A later rebuild carries the kept files, with no choice to make.
        s.edit_steps(&pack_id, |steps| toggle(steps, "hoist", false))
            .await;
        s.rebuild_until(&pack_id, "done").await;
        assert_eq!(s.read("Notes.txt"), "mine");
        assert_eq!(s.read("Readme.txt"), "edited outside");
        assert!(s.live().join("Art/T_Rock.png").is_file(), "unhoisted");
        assert_eq!(
            s.kept(&pack_id).await,
            [in_pack("Notes.txt"), in_pack("Readme.txt")]
        );
        let rows = s.rows().await;
        assert_eq!(rows[&notes], (in_pack("Notes.txt"), vec!["notes".into()]));
        assert_eq!(
            rows[&readme],
            (in_pack("Readme.txt"), vec!["readme".into()])
        );
        s.settled().await;
    }

    /// A6: discard, use the recipe's and restore put the recipe's pack back and record nothing.
    #[tokio::test]
    async fn discard_use_the_recipe_s_and_restore_put_the_recipe_back() {
        let s = setup().await;
        let (pack_id, notes, readme) = drifted(&s).await;
        let rev = s
            .engine
            .pack_steps("lib", &pack_id)
            .await
            .unwrap()
            .recipe_rev;

        let run = s.rebuild_choosing(&pack_id, last).await;
        s.wait_for(&run.id, "done").await;
        assert!(!s.live().join("Notes.txt").exists());
        assert_ne!(s.read("Readme.txt"), "edited outside");
        assert!(s.live().join("Textures/T_Log_D.png").is_file());
        assert_eq!(s.origin_of("Readme.txt").await.0, "extracted");
        let steps = s.engine.pack_steps("lib", &pack_id).await.unwrap();
        assert_eq!(steps.recipe_rev, rev, "no recipe edit");
        assert!(steps.kept.is_empty());
        let rows = s.rows().await;
        assert!(
            !rows.contains_key(&notes),
            "the discarded file's row is pruned"
        );
        assert_eq!(
            rows[&readme],
            (in_pack("Readme.txt"), vec!["readme".into()])
        );
        assert!(s.drift(&pack_id).await.is_empty());
        s.settled().await;
        s.tree_untouched().await;
    }

    /// A6: a missing `datapackage.json` turned off gives a pack without it, as an in-app delete
    /// does; a changed one kept turns the step off and carries the file as `kept`, with no clash.
    #[tokio::test]
    async fn a_missing_or_changed_datapackage_turns_the_metadata_step_off() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        std::fs::remove_file(s.live().join(metadata::FILE)).unwrap();
        assert_eq!(
            s.seen(&pack_id).await,
            [(
                metadata::FILE.into(),
                Change::Missing,
                Kind::Package,
                vec![Choice::MetadataOff, Choice::Restore]
            )]
        );
        let run = s.rebuild_choosing(&pack_id, first).await;
        s.wait_for(&run.id, "done").await;
        assert!(!s.live().join(metadata::FILE).exists());
        assert!(!s.metadata_on(&pack_id).await);
        assert!(s.drift(&pack_id).await.is_empty());
        s.settled().await;

        let s = setup().await;
        let pack_id = s.import_golden().await;
        s.write(metadata::FILE, r#"{"name": "mine"}"#);
        assert_eq!(
            s.seen(&pack_id).await,
            [(
                metadata::FILE.into(),
                Change::Changed,
                Kind::Package,
                vec![Choice::Keep, Choice::Recipe]
            )]
        );
        let run = s.rebuild_choosing(&pack_id, first).await;
        s.wait_for(&run.id, "done").await;
        assert_eq!(s.read(metadata::FILE), r#"{"name": "mine"}"#);
        assert!(!s.metadata_on(&pack_id).await);
        assert_eq!(s.origin_of(metadata::FILE).await, ("kept".into(), None));
        assert_eq!(s.kept(&pack_id).await, [in_pack(metadata::FILE)]);
        assert!(s.drift(&pack_id).await.is_empty());
        s.rebuild_until(&pack_id, "done").await;
        assert_eq!(s.read(metadata::FILE), r#"{"name": "mine"}"#);
        s.settled().await;
    }

    /// A6: with the metadata step off, a `datapackage.json` added outside OpenDAM and the step
    /// turned back on offers only discard, and the rebuild writes the recipe's.
    #[tokio::test]
    async fn an_added_file_where_the_build_writes_offers_only_discard() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        s.edit_steps(&pack_id, |steps| toggle(steps, "metadata", false))
            .await;
        s.rebuild_until(&pack_id, "done").await;
        assert!(!s.live().join(metadata::FILE).exists());

        s.write(metadata::FILE, r#"{"name": "mine"}"#);
        let keep = s.choices(&pack_id, first).await;
        assert_eq!(
            keep[0].choice,
            Choice::Keep,
            "offered while the step is off"
        );
        s.edit_steps(&pack_id, |steps| toggle(steps, "metadata", true))
            .await;
        let found = s.drift(&pack_id).await;
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].change, found[0].kind, found[0].choices.clone()),
            (Change::Added, Kind::Outside, vec![Choice::Discard])
        );
        assert!(found[0].reason.is_some());
        assert_eq!(
            s.engine
                .rebuild_pack("lib", &pack_id, keep)
                .await
                .unwrap_err(),
            STALE,
            "keep isn't offered now"
        );

        let run = s.rebuild_choosing(&pack_id, first).await;
        s.wait_for(&run.id, "done").await;
        assert_ne!(s.read(metadata::FILE), r#"{"name": "mine"}"#);
        assert_eq!(s.origin_of(metadata::FILE).await.0, "generated");
        s.settled().await;
    }

    /// A6: a changed kept file offers only keep yours, which keeps its new bytes, and a missing
    /// one only forget, which drops it from the kept files.
    #[tokio::test]
    async fn a_kept_file_changed_keeps_its_bytes_and_missing_is_only_forgotten() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        s.write("Notes.txt", "mine");
        let run = s.rebuild_choosing(&pack_id, first).await;
        s.wait_for(&run.id, "done").await;

        s.write("Notes.txt", "mine, again");
        assert_eq!(
            s.seen(&pack_id).await,
            [(
                "Notes.txt".into(),
                Change::Changed,
                Kind::Kept,
                vec![Choice::Keep]
            )]
        );
        let run = s.rebuild_choosing(&pack_id, first).await;
        s.wait_for(&run.id, "done").await;
        assert_eq!(s.read("Notes.txt"), "mine, again");

        std::fs::remove_file(s.live().join("Notes.txt")).unwrap();
        assert_eq!(
            s.seen(&pack_id).await,
            [(
                "Notes.txt".into(),
                Change::Missing,
                Kind::Kept,
                vec![Choice::Forget]
            )]
        );
        let run = s.rebuild_choosing(&pack_id, first).await;
        s.wait_for(&run.id, "done").await;
        assert!(!s.live().join("Notes.txt").exists());
        assert!(s.kept(&pack_id).await.is_empty());
        assert!(s.drift(&pack_id).await.is_empty());
        s.settled().await;
    }

    /// A4: a changed `T_Rock.png` kept while a hoist toggled off, or a `rename` added, moves it:
    /// the kept copy lands where the steps put its file, with its id and tag, its `.meta` beside
    /// it, and no row is pruned for it.
    #[tokio::test]
    async fn a_kept_file_moved_by_the_steps_keeps_its_row_and_tag() {
        for moved in ["hoist", "rename"] {
            let s = setup().await;
            let pack_id = s.import_golden().await;
            let rock = s.tag("T_Rock.png", "rock").await;
            let source = s.source_of("T_Rock.png").await;
            s.write("T_Rock.png", "my rock");
            let ids = s.ids().await;
            let to = match moved {
                "hoist" => {
                    s.edit_steps(&pack_id, |steps| toggle(steps, "hoist", false))
                        .await;
                    "Art/T_Rock.png"
                }
                _ => {
                    s.edit_steps(&pack_id, |steps| {
                        let name = "T_Stone.png".into();
                        add(steps, PlaceStep::Rename { source, name });
                    })
                    .await;
                    "T_Stone.png"
                }
            };

            let run = s.rebuild_choosing(&pack_id, |_| Choice::Keep).await;
            s.wait_for(&run.id, "done").await;
            assert_eq!(s.read(to), "my rock", "{moved}");
            assert!(!s.live().join("T_Rock.png").exists(), "{moved}");
            assert!(s.live().join(format!("{to}.meta")).is_file(), "{moved}");
            assert_eq!(s.origin_of(to).await.0, "kept", "{moved}");
            assert_eq!(s.rows().await[&rock], (in_pack(to), vec!["rock".into()]));
            assert_eq!(s.ids().await, ids, "{moved}: no row pruned or inserted");
            s.settled().await;
            s.tree_untouched().await;
        }
    }

    /// Choices that don't answer the folder now are refused before anything is queued, and a
    /// folder that changes after the Rebuild was asked for fails the build before it stages.
    #[tokio::test]
    async fn choices_for_another_folder_are_refused_and_a_later_change_fails_the_build() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        s.write("Added.txt", "mine");
        assert_eq!(
            s.engine
                .rebuild_pack("lib", &pack_id, Vec::new())
                .await
                .unwrap_err(),
            STALE
        );
        let chosen = s.choices(&pack_id, first).await;
        s.write("Added.txt", "mine, changed");
        assert_eq!(
            s.engine
                .rebuild_pack("lib", &pack_id, chosen)
                .await
                .unwrap_err(),
            STALE,
            "the choice was for other bytes"
        );
        assert_eq!(
            s.count("SELECT COUNT(*) FROM import_runs WHERE kind = 'rebuild'")
                .await,
            0
        );

        let build = s.current_build().await;
        let pause = s.engine.hooks().pause(Point::Claim);
        let run = s.rebuild_choosing(&pack_id, first).await;
        reached(&pause).await;
        s.write("Other.txt", "also mine");
        pause.release.notify_one();
        let failed = s.wait_for(&run.id, "failed").await;
        assert_eq!(
            failed.error.unwrap(),
            format!(
                "{} changed outside OpenDAM since the Rebuild was asked for; Rebuild again",
                in_pack("Other.txt")
            )
        );
        assert_eq!(s.current_build().await, build);
        assert_eq!(s.read("Added.txt"), "mine, changed");
        s.settled().await;
    }

    /// The re-check under the lock honours the choices: a kept file changed again while the
    /// build stages fails it, and the pack folder keeps the change.
    #[tokio::test]
    async fn a_kept_file_changed_during_the_build_fails_it_at_the_recheck() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        s.write("Readme.txt", "edited outside");
        let (build, rows) = (s.current_build().await, s.rows().await);
        let pause = s.engine.hooks().pause(Point::Staged);
        let run = s.rebuild_choosing(&pack_id, first).await;
        reached(&pause).await;
        s.write("Readme.txt", "edited again");
        pause.release.notify_one();

        let failed = s.wait_for(&run.id, "failed").await;
        assert_eq!(failed.error.as_deref(), Some(CHANGED));
        assert_eq!(s.current_build().await, build);
        assert_eq!(s.rows().await, rows);
        assert_eq!(s.read("Readme.txt"), "edited again");
        s.settled().await;
    }

    /// A file whose stamp changed but whose bytes didn't is no difference: the check hashes it
    /// on the mismatch and finds the manifest's sha256, so a Rebuild answers nothing.
    #[tokio::test]
    async fn a_file_rewritten_with_the_same_bytes_is_no_difference() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let path = s.live().join("Readme.txt");
        let bytes = std::fs::read(&path).unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let later = before + std::time::Duration::from_secs(5);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_ne!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );

        assert!(s.drift(&pack_id).await.is_empty());
        let run = s
            .engine
            .rebuild_pack("lib", &pack_id, Vec::new())
            .await
            .unwrap();
        s.wait_for(&run.id, "done").await;
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        s.settled().await;
    }

    /// A kept file of its own at a path the build comes to write fails the build, naming it,
    /// and changes nothing: the build's own file can't take the kept file's path.
    #[tokio::test]
    async fn a_kept_file_where_the_build_now_writes_fails_the_build_and_changes_nothing() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        // With `hoist Art` on there is no `Art/` folder: a file added there is kept as is.
        std::fs::create_dir_all(s.live().join("Art")).unwrap();
        s.write("Art/T_Rock.png", "my own rock");
        let run = s.rebuild_choosing(&pack_id, |_| Choice::Keep).await;
        s.wait_for(&run.id, "done").await;
        assert_eq!(s.origin_of("Art/T_Rock.png").await, ("kept".into(), None));

        // With the hoist off, the recipe's T_Rock.png lands at that path.
        s.edit_steps(&pack_id, |steps| toggle(steps, "hoist", false))
            .await;
        let (build, rows) = (s.current_build().await, s.rows().await);
        let failed = s.rebuild_until(&pack_id, "failed").await;
        assert_eq!(
            failed.error.unwrap(),
            format!(
                "{} is kept from outside changes where the build now puts a file of the \
                 recipe: rename or delete it in the app, then Rebuild again",
                in_pack("Art/T_Rock.png")
            )
        );
        assert_eq!(s.current_build().await, build);
        assert_eq!(s.rows().await, rows);
        assert_eq!(s.read("Art/T_Rock.png"), "my own rock");
        assert!(
            s.live().join("T_Rock.png").is_file(),
            "the live folder is unchanged"
        );
        assert_eq!(s.kept(&pack_id).await, [in_pack("Art/T_Rock.png")]);
        s.settled().await;
    }

    /// A missing file and its missing `.meta` both left out record one `exclude`, the asset's,
    /// which drops its `.meta` too, so the rebuild builds.
    #[tokio::test]
    async fn excluding_a_missing_file_and_its_meta_records_one_step() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let source = s.source_of("Textures/T_Log_D.png").await;
        for path in ["Textures/T_Log_D.png", "Textures/T_Log_D.png.meta"] {
            std::fs::remove_file(s.live().join(path)).unwrap();
        }
        let before = s.engine.pack_steps("lib", &pack_id).await.unwrap();
        let chosen = s.choices(&pack_id, first).await;
        assert_eq!(chosen.len(), 2);
        assert!(chosen.iter().all(|c| c.choice == Choice::Exclude));

        let run = s
            .engine
            .rebuild_pack("lib", &pack_id, chosen)
            .await
            .unwrap();
        let after = s.engine.pack_steps("lib", &pack_id).await.unwrap();
        assert_eq!(after.recipe_rev, before.recipe_rev + 1);
        assert_eq!(after.steps[..before.steps.len()], before.steps[..]);
        assert_eq!(
            after.steps[before.steps.len()..],
            [Step {
                kind: "exclude".into(),
                enabled: true,
                options: serde_json::json!({ "source": source }),
            }]
        );
        s.wait_for(&run.id, "done").await;
        assert!(
            !s.live().join("Textures").exists(),
            "the emptied folder goes too"
        );
        assert!(s.drift(&pack_id).await.is_empty());
        s.settled().await;
    }

    /// A kept `datapackage.json` once the metadata step is turned back on, and a kept copy an
    /// `exclude` now leaves out, are listed as clashes offering only discard, rather than
    /// failing every Rebuild: discarding lets the recipe's file in, or leaves the file out.
    #[tokio::test]
    async fn a_kept_file_the_recipe_now_writes_over_or_leaves_out_offers_only_discard() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        s.write(metadata::FILE, r#"{"name": "mine"}"#);
        s.write("Readme.txt", "edited outside");
        let run = s.rebuild_choosing(&pack_id, first).await;
        s.wait_for(&run.id, "done").await;
        assert_eq!(
            s.kept(&pack_id).await,
            [in_pack("Readme.txt"), in_pack(metadata::FILE)]
        );
        assert!(!s.metadata_on(&pack_id).await);

        let readme = s.source_of("Readme.txt").await;
        s.edit_steps(&pack_id, |steps| {
            toggle(steps, "metadata", true);
            add(steps, PlaceStep::Exclude { source: readme });
        })
        .await;
        let found = s.drift(&pack_id).await;
        assert_eq!(
            found
                .iter()
                .map(|d| (d.path.clone(), d.change, d.kind, d.choices.clone()))
                .collect::<Vec<_>>(),
            [
                (
                    in_pack("Readme.txt"),
                    Change::Clash,
                    Kind::Kept,
                    vec![Choice::Discard]
                ),
                (
                    in_pack(metadata::FILE),
                    Change::Clash,
                    Kind::Kept,
                    vec![Choice::Discard]
                ),
            ]
        );
        assert!(found.iter().all(|d| d.reason.is_some()));
        assert_eq!(
            s.engine
                .rebuild_pack("lib", &pack_id, Vec::new())
                .await
                .unwrap_err(),
            STALE,
            "a Rebuild must answer them"
        );

        let run = s.rebuild_choosing(&pack_id, first).await;
        s.wait_for(&run.id, "done").await;
        assert!(
            !s.live().join("Readme.txt").exists(),
            "left out by its exclude"
        );
        assert_ne!(s.read(metadata::FILE), r#"{"name": "mine"}"#);
        assert_eq!(s.origin_of(metadata::FILE).await.0, "generated");
        assert!(s.kept(&pack_id).await.is_empty());
        assert!(s.drift(&pack_id).await.is_empty());
        s.settled().await;
    }

    /// A Rebuild whose "Leave it out" or "Turn the metadata step off" was recorded, then failed,
    /// leaves the file missing: a later Restore is honoured, turning that edit back.
    #[tokio::test]
    async fn restore_after_a_failed_rebuild_undoes_the_edit_the_failed_run_recorded() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let log = s.source_of("Textures/T_Log_D.png").await;
        std::fs::remove_file(s.live().join("Textures/T_Log_D.png")).unwrap();
        std::fs::remove_file(s.live().join(metadata::FILE)).unwrap();
        s.engine.hooks().fail_once(Point::Staged);
        let run = s.rebuild_choosing(&pack_id, first).await;
        s.wait_for(&run.id, "failed").await;
        let steps = s.engine.pack_steps("lib", &pack_id).await.unwrap().steps;
        let excluding = |steps: &[Step]| {
            steps.iter().any(|st| {
                st.kind == "exclude" && st.enabled && st.options["source"] == serde_json::json!(log)
            })
        };
        assert!(excluding(&steps), "the failed run's exclude stays");
        assert!(!s.metadata_on(&pack_id).await);

        let restore = |d: &Difference| {
            assert!(d.choices.contains(&Choice::Restore), "{d:?}");
            Choice::Restore
        };
        let run = s.rebuild_choosing(&pack_id, restore).await;
        s.wait_for(&run.id, "done").await;
        assert!(s.live().join("Textures/T_Log_D.png").is_file());
        assert!(s.live().join(metadata::FILE).is_file());
        let steps = s.engine.pack_steps("lib", &pack_id).await.unwrap().steps;
        assert!(!excluding(&steps), "Restore turned the exclude off");
        assert!(
            s.metadata_on(&pack_id).await,
            "and the metadata step back on"
        );
        assert!(s.drift(&pack_id).await.is_empty());
        s.settled().await;
    }

    /// A pack folder deleted outside OpenDAM defaults every missing file to Restore, so the
    /// default Rebuild brings the pack back rather than leaving everything out.
    #[tokio::test]
    async fn a_deleted_pack_folder_defaults_every_file_to_restore() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let tree = crate::import::unitypackage::tests::on_disk(&s.live());
        std::fs::remove_dir_all(s.live()).unwrap();
        let found = s.drift(&pack_id).await;
        assert_eq!(found.len(), tree.len());
        assert!(
            found.iter().all(|d| d.choices[0] == Choice::Restore),
            "{found:?}"
        );

        let run = s.rebuild_choosing(&pack_id, first).await;
        s.wait_for(&run.id, "done").await;
        assert_eq!(crate::import::unitypackage::tests::on_disk(&s.live()), tree);
        assert!(s.drift(&pack_id).await.is_empty());
        s.settled().await;
    }

    /// A changed file from the original whose exclude is on, and a kept `.meta` whose asset's
    /// exclude is on, can't be kept: each is a clash offering only discard, which leaves the
    /// exclude as it is, and the Rebuild then builds.
    #[tokio::test]
    async fn a_file_an_exclude_now_leaves_out_offers_only_discard() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let (readme, log) = (
            s.source_of("Readme.txt").await,
            s.source_of("Textures/T_Log_D.png").await,
        );
        s.write("Textures/T_Log_D.png.meta", "my meta");
        let run = s.rebuild_choosing(&pack_id, first).await;
        s.wait_for(&run.id, "done").await;
        assert_eq!(
            s.kept(&pack_id).await,
            [in_pack("Textures/T_Log_D.png.meta")]
        );

        s.write("Readme.txt", "edited outside");
        s.edit_steps(&pack_id, |steps| {
            add(steps, PlaceStep::Exclude { source: readme });
            add(steps, PlaceStep::Exclude { source: log });
        })
        .await;
        assert_eq!(
            s.seen(&pack_id).await,
            [
                (
                    "Readme.txt".into(),
                    Change::Clash,
                    Kind::Extracted,
                    vec![Choice::Discard]
                ),
                (
                    "Textures/T_Log_D.png.meta".into(),
                    Change::Clash,
                    Kind::Kept,
                    vec![Choice::Discard]
                ),
            ]
        );
        let steps = s.engine.pack_steps("lib", &pack_id).await.unwrap();
        let run = s.rebuild_choosing(&pack_id, first).await;
        s.wait_for(&run.id, "done").await;
        assert!(!s.live().join("Readme.txt").exists());
        assert!(!s.live().join("Textures/T_Log_D.png.meta").exists());
        assert!(s.kept(&pack_id).await.is_empty());
        let after = s.engine.pack_steps("lib", &pack_id).await.unwrap();
        assert_eq!(after.steps, steps.steps, "discard leaves the steps alone");
        assert!(s.drift(&pack_id).await.is_empty());
        s.settled().await;
    }

    /// A Restore that would turn back a step you set yourself (an exclude you turned on, the
    /// metadata step you turned off) is refused, naming the file, and changes no step.
    #[tokio::test]
    async fn restore_never_turns_back_a_step_you_set() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let log = s.source_of("Textures/T_Log_D.png").await;
        std::fs::remove_file(s.live().join("Textures/T_Log_D.png")).unwrap();
        std::fs::remove_file(s.live().join(metadata::FILE)).unwrap();
        s.edit_steps(&pack_id, |steps| {
            add(steps, PlaceStep::Exclude { source: log });
            toggle(steps, "metadata", false);
        })
        .await;
        let steps = s.engine.pack_steps("lib", &pack_id).await.unwrap();

        for path in ["Textures/T_Log_D.png", metadata::FILE] {
            let chosen = s
                .choices(&pack_id, |d| {
                    if d.path == in_pack(path) {
                        Choice::Restore
                    } else {
                        d.choices[0]
                    }
                })
                .await;
            let refused = s
                .engine
                .rebuild_pack("lib", &pack_id, chosen)
                .await
                .unwrap_err();
            assert!(
                refused.contains(&in_pack(path)) && refused.contains("can't be restored"),
                "{refused}"
            );
        }
        assert_eq!(s.engine.pack_steps("lib", &pack_id).await.unwrap(), steps);
        assert_eq!(
            s.count("SELECT COUNT(*) FROM import_runs WHERE kind = 'rebuild'")
                .await,
            0
        );
    }

    /// After a run that recorded "Leave it out" for a file and its `.meta` failed, a Restore of
    /// both turns its own exclude back and brings both back; a Restore of the file with its
    /// `.meta` left out records the `.meta`'s own exclude, so the `.meta` stays out.
    #[tokio::test]
    async fn restore_after_a_failed_rebuild_judges_a_meta_by_the_excludes_it_leaves() {
        for meta_choice in [Choice::Restore, Choice::Exclude] {
            let s = setup().await;
            let pack_id = s.import_golden().await;
            let log = s.source_of("Textures/T_Log_D.png").await;
            let meta = s.source_of("Textures/T_Log_D.png.meta").await;
            for path in ["Textures/T_Log_D.png", "Textures/T_Log_D.png.meta"] {
                std::fs::remove_file(s.live().join(path)).unwrap();
            }
            s.engine.hooks().fail_once(Point::Staged);
            let run = s.rebuild_choosing(&pack_id, first).await;
            s.wait_for(&run.id, "failed").await;

            let meta_path = in_pack("Textures/T_Log_D.png.meta");
            let run = s
                .rebuild_choosing(&pack_id, |d| {
                    if d.path == meta_path {
                        meta_choice
                    } else {
                        Choice::Restore
                    }
                })
                .await;
            s.wait_for(&run.id, "done").await;
            assert!(
                s.live().join("Textures/T_Log_D.png").is_file(),
                "{meta_choice:?}"
            );
            assert_eq!(
                s.live().join("Textures/T_Log_D.png.meta").is_file(),
                meta_choice == Choice::Restore,
                "{meta_choice:?}"
            );
            let steps = s.engine.pack_steps("lib", &pack_id).await.unwrap().steps;
            let excludes = |source: &Vec<String>| {
                steps.iter().any(|st| {
                    st.kind == "exclude"
                        && st.enabled
                        && st.options["source"] == serde_json::json!(source)
                })
            };
            assert!(
                !excludes(&log),
                "{meta_choice:?}: the failed run's exclude is turned back"
            );
            assert_eq!(
                excludes(&meta),
                meta_choice == Choice::Exclude,
                "{meta_choice:?}"
            );
            assert!(s.drift(&pack_id).await.is_empty(), "{meta_choice:?}");
            s.settled().await;
        }
    }
}
