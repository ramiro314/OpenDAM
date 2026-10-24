//! In-app renames and deletes inside an imported pack (plan pack-import, Edits after an import,
//! A5): the steps `rename_asset` and `delete_assets` record, the `.meta` they move or delete
//! beside each file, and rebuilds that keep them.
//!
//! The tests sit in a `mod tests` block, as in every other file, so source scans that stop there
//! (`structure::tests::metadata_writers`) read none of them.

mod tests {
    use std::cell::Cell;
    use std::path::Path;

    use serde_json::{json, Value};

    use super::super::rebuild_tests::tests::{in_pack, reached};
    use crate::commands::asset::{delete_assets_with, rename_asset_with};
    use crate::export::blender::test_blender;
    use crate::import::engine::tests::{setup, Setup};
    use crate::import::engine::*;

    /// The synthetic package's prefix to every source path.
    const WRAP: &str = "Assets/Pack/Built-In/Built-In_Pack/Pack";
    /// The nested package's `T_Rock.png`, below the pack root once flattened and `Art` hoisted.
    const NESTED_ROCK: &str = "Houses/Built-In/Built-In_Pack/Pack/Art/T_Rock.png";

    fn wrapped(path: &str) -> Vec<String> {
        vec![format!("{WRAP}/{path}")]
    }

    fn step(kind: &str, options: Value) -> Step {
        Step {
            kind: kind.into(),
            enabled: true,
            options,
        }
    }

    impl Setup {
        /// Renames the item at `path` in the pack to `name`, renaming files with `rename`.
        async fn rename_with(
            &self,
            path: &str,
            name: &str,
            rename: impl Fn(&Path, &Path) -> std::io::Result<()>,
        ) -> Result<(), String> {
            let id = self.id_at(path).await;
            rename_asset_with(&self.engine.db, &id, name, rename).await
        }

        async fn rename(&self, path: &str, name: &str) -> Result<(), String> {
            self.rename_with(path, name, |from, to| std::fs::rename(from, to))
                .await
        }

        /// Deletes the item at `path` in the pack, as `delete_assets` does with the trash.
        async fn delete(&self, path: &str) -> Result<(), String> {
            self.delete_with(path, |p: &Path| std::fs::remove_file(p))
                .await
        }

        /// Deletes the item at `path` in the pack, trashing files with `trash`.
        async fn delete_with(
            &self,
            path: &str,
            trash: impl Fn(&Path) -> std::io::Result<()>,
        ) -> Result<(), String> {
            let id = self.id_at(path).await;
            delete_assets_with(&self.engine.db, &[id], true, trash)
                .await
                .map_err(|e| format!("{e:?}"))
        }

        /// Whether the library holds an asset row at `path` in the pack.
        async fn has_row(&self, path: &str) -> bool {
            self.count(&format!(
                "SELECT COUNT(*) FROM assets WHERE file_path = '{}'",
                in_pack(path)
            ))
            .await
                == 1
        }

        /// Queues another machine's rebuild of the pack, `race`, whenever `event` (`UPDATE OF
        /// file_path` or `DELETE`) touches an asset row: a run queued between a command's check
        /// and its transaction, which the command's own transaction then sees.
        async fn queue_a_run_on(&self, event: &str) {
            sqlx::query(&format!(
                "CREATE TRIGGER race AFTER {event} ON assets BEGIN
                 INSERT OR IGNORE INTO import_runs (id, kind, packs, status, created_at,
                 owner_machine, owner_boot, owner_pid, owner_started, heartbeat_at)
                 SELECT 'race', 'rebuild',
                 json_array(json_object('packId', id, 'recipeRev', recipe_rev)), 'queued', 0,
                 'another machine', 'boot', 1, 1, 9000000000000000 FROM import_packs; END"
            ))
            .execute(&self.lib.pool)
            .await
            .unwrap();
        }

        fn exists(&self, path: &str) -> bool {
            self.live().join(path).exists()
        }

        async fn steps_of(&self, pack_id: &str) -> PackSteps {
            self.engine.pack_steps("lib", pack_id).await.unwrap()
        }

        /// The steps recorded after `before`'s, checking `before`'s are as they were.
        async fn recorded(&self, pack_id: &str, before: &PackSteps) -> Vec<Step> {
            let now = self.steps_of(pack_id).await.steps;
            assert_eq!(now[..before.steps.len()], before.steps[..]);
            now[before.steps.len()..].to_vec()
        }

        /// Whether the current build's manifest holds `path` in the pack.
        async fn listed(&self, path: &str) -> bool {
            self.count(&format!(
                "SELECT COUNT(*) FROM import_outputs o JOIN import_packs p ON p.id = o.pack_id
                 AND o.build_id = p.current_build WHERE o.path = '{}'",
                in_pack(path)
            ))
            .await
                == 1
        }

        /// `path` and its `.meta` are on disk and in the manifest, or neither is.
        async fn with_meta(&self, path: &str, present: bool) {
            for path in [path.to_string(), format!("{path}.meta")] {
                assert_eq!(self.exists(&path), present, "{path} on disk");
                assert_eq!(self.listed(&path).await, present, "{path} in the manifest");
            }
        }
    }

    /// A5: a rename and a delete append `rename` and `exclude` steps by source path, move or
    /// delete the `.meta` with the file and their manifest rows with them, so the rebuild's check
    /// of the pack folder finds no difference, and the rebuild keeps the new name, the row and its
    /// tag, and doesn't bring the deleted file back.
    #[tokio::test]
    async fn a_rename_and_a_delete_record_steps_take_the_meta_along_and_survive_a_rebuild() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let before = s.steps_of(&pack_id).await;
        let rock = s.tag("T_Rock.png", "rock").await;

        s.rename("T_Rock.png", "T_Stone").await.unwrap();
        s.with_meta("T_Stone.png", true).await;
        s.with_meta("T_Rock.png", false).await;
        assert_eq!(
            s.source_of("T_Stone.png.meta").await,
            wrapped("Art/T_Rock.png.meta")
        );

        s.delete("Materials/M_LogBark.mat").await.unwrap();
        s.with_meta("Materials/M_LogBark.mat", false).await;

        assert_eq!(
            s.recorded(&pack_id, &before).await,
            [
                step(
                    "rename",
                    json!({"source": wrapped("Art/T_Rock.png"), "name": "T_Stone.png"})
                ),
                step(
                    "exclude",
                    json!({"source": wrapped("Art/Materials/M_LogBark.mat")})
                ),
            ]
        );
        assert_eq!(s.steps_of(&pack_id).await.recipe_rev, before.recipe_rev + 2);

        s.rebuild_until(&pack_id, "done").await;
        s.with_meta("T_Stone.png", true).await;
        s.with_meta("T_Rock.png", false).await;
        s.with_meta("Materials/M_LogBark.mat", false).await;
        assert!(!s.exists("Materials"), "the emptied folder goes too");
        assert_eq!(s.id_at("T_Stone.png").await, rock);
        assert_eq!(
            s.rows().await[&rock],
            (in_pack("T_Stone.png"), vec!["rock".to_string()])
        );
        s.settled().await;
        s.tree_untouched().await;
    }

    /// A5: the nested package's `T_Rock.png` is renamed by its two-element source path, so the
    /// rebuild renames it alone and the outer `T_Rock.png` keeps its name.
    #[tokio::test]
    async fn the_nested_t_rock_is_renamed_by_its_two_element_source_path() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let before = s.steps_of(&pack_id).await;
        let source = s.source_of(NESTED_ROCK).await;
        assert_eq!(source.len(), 2, "{source:?}");

        s.rename(NESTED_ROCK, "T_Pebble.png").await.unwrap();
        assert_eq!(
            s.recorded(&pack_id, &before).await,
            [step(
                "rename",
                json!({"source": source, "name": "T_Pebble.png"})
            )]
        );

        s.rebuild_until(&pack_id, "done").await;
        let pebble = NESTED_ROCK.replace("T_Rock", "T_Pebble");
        s.with_meta(&pebble, true).await;
        s.with_meta(NESTED_ROCK, false).await;
        s.with_meta("T_Rock.png", true).await;
        assert_eq!(s.source_of(&pebble).await, source);
        s.settled().await;
    }

    /// A5: renaming the pack's `datapackage.json` is refused and changes nothing; deleting it
    /// turns the metadata step off, and the rebuild doesn't write it back.
    #[tokio::test]
    async fn datapackage_json_can_t_be_renamed_and_its_delete_turns_the_metadata_step_off() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let before = s.steps_of(&pack_id).await;

        let refused = s.rename("datapackage.json", "package.json").await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.contains("edit the pack's metadata step instead")),
            "{refused:?}"
        );
        assert!(s.exists("datapackage.json") && s.listed("datapackage.json").await);
        assert_eq!(s.steps_of(&pack_id).await, before);

        s.delete("datapackage.json").await.unwrap();
        assert!(!s.exists("datapackage.json") && !s.listed("datapackage.json").await);
        let after = s.steps_of(&pack_id).await;
        assert_eq!(after.recipe_rev, before.recipe_rev + 1);
        let mut expected = before.steps.clone();
        expected
            .iter_mut()
            .find(|s| s.kind == "metadata")
            .unwrap()
            .enabled = false;
        assert_eq!(after.steps, expected, "only the metadata step changed");

        s.rebuild_until(&pack_id, "done").await;
        assert!(!s.exists("datapackage.json"));
        s.settled().await;
    }

    /// A5: while a run for the pack is queued, then running, a rename and a delete inside it are
    /// refused before any file moves.
    #[tokio::test]
    async fn edits_are_refused_while_a_run_for_the_pack_is_queued_or_running() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let before = s.steps_of(&pack_id).await;
        let claim = s.engine.hooks().pause(Point::Claim);
        let run = s.rebuild(&pack_id).await;
        reached(&claim).await;
        assert_eq!(s.steps_of(&pack_id).await.recipe_rev, before.recipe_rev);

        let refused = Err(BUSY.to_string());
        let unchanged = |s: &Setup| {
            assert!(s.exists("T_Rock.png") && s.exists("T_Rock.png.meta"));
            assert!(
                s.exists("Materials/M_LogBark.mat") && s.exists("Materials/M_LogBark.mat.meta")
            );
            assert!(!s.exists("T_Stone.png"));
        };
        assert_eq!(s.rename("T_Rock.png", "T_Stone.png").await, refused);
        assert!(s
            .delete("Materials/M_LogBark.mat")
            .await
            .is_err_and(|e| e.contains(BUSY)));
        unchanged(&s);

        let staged = s.engine.hooks().pause(Point::Staged);
        claim.release.notify_one();
        reached(&staged).await;
        assert_eq!(s.rename("T_Rock.png", "T_Stone.png").await, refused);
        assert!(s
            .delete("Materials/M_LogBark.mat")
            .await
            .is_err_and(|e| e.contains(BUSY)));
        unchanged(&s);
        assert_eq!(s.steps_of(&pack_id).await, before, "nothing was recorded");

        staged.release.notify_one();
        s.wait_for(&run.id, "done").await;
        s.with_meta("T_Rock.png", true).await;
    }

    /// A5: a rename whose second file (the `.meta`) fails to move, and one the library DB refuses
    /// once every file moved, leave the file and its `.meta` under their old names, and record
    /// nothing.
    #[tokio::test]
    async fn a_failed_rename_leaves_the_file_and_its_meta_under_their_old_names() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let before = s.steps_of(&pack_id).await;
        let rock = s.id_at("T_Rock.png").await;

        let calls = Cell::new(0);
        let failed = s
            .rename_with("T_Rock.png", "T_Stone.png", |from, to| {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    return Err(std::io::Error::other("injected"));
                }
                std::fs::rename(from, to)
            })
            .await;
        assert!(failed.is_err_and(|e| e.contains("injected")));
        assert_eq!(
            calls.get(),
            3,
            "the file moved, its .meta failed, the file went back"
        );
        unrenamed(&s, &pack_id, &before).await;

        sqlx::query(
            "CREATE TRIGGER refuse BEFORE UPDATE OF path ON import_outputs
             BEGIN SELECT RAISE(ABORT, 'busy'); END",
        )
        .execute(&s.lib.pool)
        .await
        .unwrap();
        let failed = s.rename("T_Rock.png", "T_Stone.png").await;
        assert!(failed.is_err_and(|e| e.contains("so it was undone")));
        unrenamed(&s, &pack_id, &before).await;
        assert_eq!(s.id_at("T_Rock.png").await, rock);
    }

    /// `T_Rock.png` and its `.meta` are under their old names, and no step was recorded.
    async fn unrenamed(s: &Setup, pack_id: &str, before: &PackSteps) {
        s.with_meta("T_Rock.png", true).await;
        assert!(!s.exists("T_Stone.png") && !s.exists("T_Stone.png.meta"));
        assert_eq!(&s.steps_of(pack_id).await, before);
    }

    /// A5 under Blender: the `SM_Log` group (the FBX and its generated GLB) records one `rename`
    /// for the FBX, moving both files and the FBX's `.meta`, and the rebuild makes both under the
    /// new stem; a rename failing midway puts them all back; deleting the group records one
    /// `exclude`, and the rebuild makes neither file.
    #[tokio::test]
    async fn the_sm_log_group_records_one_step_for_its_fbx() {
        let s = setup().await;
        let Some(blender) = test_blender() else {
            return;
        };
        crate::export::settings::update(
            &s.engine.db.master_pool,
            json!({ "blenderPath": blender }),
        )
        .await
        .unwrap();
        let pack_id = s.import_golden().await;
        let before = s.steps_of(&pack_id).await;
        let fbx = wrapped("Art/Models/SM_Log.fbx");
        assert!(s.exists("Models/SM_Log.glb"));

        // The GLB is the group's main item, so it moves first, then the FBX, then its .meta.
        let failed = s
            .rename_with("Models/SM_Log.glb", "SM_Trunk", |from, to| {
                if from.extension().is_some_and(|e| e == "meta") {
                    return Err(std::io::Error::other("injected"));
                }
                std::fs::rename(from, to)
            })
            .await;
        assert!(failed.is_err_and(|e| e.contains("injected")));
        s.with_meta("Models/SM_Log.fbx", true).await;
        assert!(s.exists("Models/SM_Log.glb") && !s.exists("Models/SM_Trunk.glb"));
        assert!(!s.exists("Models/SM_Trunk.fbx"));
        assert_eq!(s.steps_of(&pack_id).await, before);

        let glb = s.id_at("Models/SM_Log.glb").await;
        s.rename("Models/SM_Log.glb", "SM_Trunk").await.unwrap();
        s.with_meta("Models/SM_Trunk.fbx", true).await;
        s.with_meta("Models/SM_Log.fbx", false).await;
        assert!(s.exists("Models/SM_Trunk.glb") && s.listed("Models/SM_Trunk.glb").await);
        assert_eq!(
            s.recorded(&pack_id, &before).await,
            [step(
                "rename",
                json!({"source": fbx, "name": "SM_Trunk.fbx"})
            )]
        );

        s.rebuild_until(&pack_id, "done").await;
        s.with_meta("Models/SM_Trunk.fbx", true).await;
        assert!(s.exists("Models/SM_Trunk.glb"));
        assert!(!s.exists("Models/SM_Log.glb") && !s.exists("Models/SM_Log.fbx"));
        assert_eq!(s.id_at("Models/SM_Trunk.glb").await, glb);

        let renamed = s.steps_of(&pack_id).await;
        s.delete("Models/SM_Trunk.glb").await.unwrap();
        s.with_meta("Models/SM_Trunk.fbx", false).await;
        assert!(!s.exists("Models/SM_Trunk.glb") && !s.listed("Models/SM_Trunk.glb").await);
        assert_eq!(
            s.recorded(&pack_id, &renamed).await,
            [step("exclude", json!({"source": fbx}))]
        );

        s.rebuild_until(&pack_id, "done").await;
        assert!(!s.exists("Models"), "neither file comes back");
        s.settled().await;
    }

    /// A run queued after a rename's check, seen only by its transaction (`edits::record`):
    /// the rename is refused and undone, every file and `.meta` under its old name, and nothing
    /// is recorded, the run's row going with the rolled-back transaction.
    #[tokio::test]
    async fn a_run_queued_during_a_rename_refuses_it_and_puts_the_files_back() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let before = s.steps_of(&pack_id).await;
        let rock = s.id_at("T_Rock.png").await;
        s.queue_a_run_on("UPDATE OF file_path").await;

        let refused = s.rename("T_Rock.png", "T_Stone.png").await;
        assert!(
            refused.as_ref().is_err_and(|e| e.contains(BUSY)),
            "{refused:?}"
        );
        unrenamed(&s, &pack_id, &before).await;
        assert_eq!(s.id_at("T_Rock.png").await, rock);
        assert_eq!(
            s.count("SELECT COUNT(*) FROM import_runs WHERE id = 'race'")
                .await,
            0
        );
    }

    /// A run queued after a delete's check, seen only by its transaction: the files are already
    /// in the trash, so their rows go all the same, but no step is recorded and `recipe_rev`
    /// stays, the manifest still listing the files for the run's re-check to stop on.
    #[tokio::test]
    async fn a_run_queued_during_a_delete_drops_the_rows_but_records_no_step() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let before = s.steps_of(&pack_id).await;
        s.queue_a_run_on("DELETE").await;

        s.delete("Materials/M_LogBark.mat").await.unwrap();
        assert!(!s.exists("Materials/M_LogBark.mat"));
        assert!(!s.exists("Materials/M_LogBark.mat.meta"));
        assert!(!s.has_row("Materials/M_LogBark.mat").await);
        assert!(s.listed("Materials/M_LogBark.mat").await);
        assert!(s.listed("Materials/M_LogBark.mat.meta").await);
        assert_eq!(s.steps_of(&pack_id).await, before);
        assert_eq!(
            s.count("SELECT COUNT(*) FROM import_runs WHERE id = 'race'")
                .await,
            1
        );
    }

    /// A delete whose `.meta` fails to go to the trash: the file is gone, with its row and its
    /// manifest row, and its `exclude` is recorded; the `.meta` stays on disk and in the
    /// manifest, the error naming it; and the rebuild, whose step drops both, succeeds.
    #[tokio::test]
    async fn a_delete_whose_meta_fails_to_trash_keeps_the_meta_and_records_the_file() {
        let s = setup().await;
        let pack_id = s.import_golden().await;
        let before = s.steps_of(&pack_id).await;

        let failed = s
            .delete_with("Materials/M_LogBark.mat", |p: &Path| {
                if p.extension().is_some_and(|e| e == "meta") {
                    return Err(std::io::Error::other("injected"));
                }
                std::fs::remove_file(p)
            })
            .await;
        assert!(
            failed
                .as_ref()
                .is_err_and(|e| e.contains("M_LogBark.mat.meta") && e.contains("injected")),
            "{failed:?}"
        );
        assert!(!s.exists("Materials/M_LogBark.mat"));
        assert!(!s.listed("Materials/M_LogBark.mat").await);
        assert!(!s.has_row("Materials/M_LogBark.mat").await);
        assert!(s.exists("Materials/M_LogBark.mat.meta"));
        assert!(s.listed("Materials/M_LogBark.mat.meta").await);
        assert_eq!(
            s.recorded(&pack_id, &before).await,
            [step(
                "exclude",
                json!({"source": wrapped("Art/Materials/M_LogBark.mat")})
            )]
        );

        s.rebuild_until(&pack_id, "done").await;
        s.with_meta("Materials/M_LogBark.mat", false).await;
        s.settled().await;
    }
}
