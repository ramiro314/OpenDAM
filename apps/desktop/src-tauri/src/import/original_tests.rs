//! The original's state through the engine (plan pack-import, The original's state, A15): the
//! check at startup and when the Imports view opens, Rebuild refused while the original is
//! missing or changed, and Locate original.
//!
//! The tests sit in a `mod tests` block, as in every other file, so source scans that stop there
//! (`structure::tests::metadata_writers`) read none of them.

mod tests {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::mpsc;

    use crate::import::engine::tests::{setup, setup_with, Setup, TARGET};
    use crate::import::engine::*;
    use crate::import::original::{Located, CHANGED, MISSING};
    use crate::import::unitypackage::tests::fixture;

    /// The extracted path, below the pack root, of the file `changed_package` adds.
    const ADDED: &str = "T_Added.png";

    /// `synthetic.unitypackage` with one more image in its `Art` folder.
    fn changed_package(to: &Path) {
        let mut gz = Vec::new();
        flate2::read::GzDecoder::new(
            std::fs::File::open(fixture("synthetic.unitypackage")).unwrap(),
        )
        .read_to_end(&mut gz)
        .unwrap();
        let mut archive = tar::Archive::new(gz.as_slice());
        let mut builder = tar::Builder::new(Vec::new());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let header = entry.header().clone();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            builder.append(&header, data.as_slice()).unwrap();
        }
        let pathname = format!("Assets/Pack/Built-In/Built-In_Pack/Pack/Art/{ADDED}");
        for (name, data) in [
            ("f00d/asset", b"added".as_slice()),
            ("f00d/pathname", pathname.as_bytes()),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            builder.append_data(&mut header, name, data).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut out = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut out, &tar).unwrap();
        std::fs::write(to, out.finish().unwrap()).unwrap();
    }

    struct Original {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    impl Setup {
        /// Imports the golden pack from a copy of `synthetic.unitypackage` outside the library,
        /// built, and returns the copy and the pack's id.
        async fn import_copy(&self) -> (Original, String) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("synthetic.unitypackage");
            std::fs::copy(fixture("synthetic.unitypackage"), &path).unwrap();
            let mut request = self.golden_request();
            request.original_path = path.to_string_lossy().into_owned();
            let run = self.engine.start_import(request).await.unwrap();
            self.wait_for(&run.id, "done").await;
            let path = path.canonicalize().unwrap();
            (Original { _dir: dir, path }, run.packs[0].pack_id.clone())
        }

        /// The library's one import's `(original_path, original_state, changed_sha256)`, once
        /// the Imports view's check ran.
        async fn checked(&self) -> (String, String, Option<String>) {
            let imports = self.engine.check_originals("lib").await.unwrap();
            let [import] = imports.as_slice() else {
                panic!("{imports:?}")
            };
            (
                import.original_path.clone(),
                import.original_state.clone(),
                import.changed_sha256.clone(),
            )
        }

        fn announced(&self) -> bool {
            self.events
                .lock()
                .unwrap()
                .contains(&ImportEvent::OriginalsChanged("lib".into()))
        }

        async fn locate(
            &self,
            original: &Path,
            path: &Path,
            named: Option<&str>,
        ) -> Result<Located, String> {
            self.engine
                .locate_original(
                    "lib",
                    &original.to_string_lossy(),
                    &path.to_string_lossy(),
                    named,
                )
                .await
        }
    }

    fn sha256(path: &Path) -> String {
        crate::export::hashes::sha256_file(path).unwrap()
    }

    #[tokio::test]
    async fn a_moved_original_is_missing_and_refuses_rebuild_until_located_by_its_sha256() {
        let s = setup().await;
        let (original, pack) = s.import_copy().await;
        let at = original.path.to_string_lossy().into_owned();
        assert_eq!(s.checked().await, (at.clone(), "ok".into(), None));
        assert!(!s.announced());

        let moved = original
            .path
            .parent()
            .unwrap()
            .join("Moved/synthetic.unitypackage");
        std::fs::create_dir(moved.parent().unwrap()).unwrap();
        std::fs::rename(&original.path, &moved).unwrap();
        assert_eq!(s.checked().await, (at.clone(), "missing".into(), None));
        assert!(
            s.announced(),
            "the Imports view and the Preparation panel refetch"
        );
        assert_eq!(
            s.engine.rebuild_pack("lib", &pack).await.unwrap_err(),
            MISSING
        );
        // The pack keeps working: its files and rows are where the build put them.
        assert!(s.live().join("datapackage.json").is_file());

        // Another package isn't the original: nothing changes until Use this file names it.
        let other = fixture("escape.unitypackage");
        let differs = s.locate(&original.path, &other, None).await.unwrap();
        assert_eq!(
            differs,
            Located::Differs {
                path: other.canonicalize().unwrap().to_string_lossy().into_owned(),
                sha256: sha256(&other),
            }
        );
        let zip = fixture("kenney.zip");
        let error = s.locate(&original.path, &zip, None).await.unwrap_err();
        assert_eq!(
            error,
            "The original is a .unitypackage: locate a .unitypackage file"
        );
        assert_eq!(s.checked().await, (at.clone(), "missing".into(), None));

        let located = s.locate(&original.path, &moved, None).await.unwrap();
        assert_eq!(located, Located::Located);
        let moved = moved.to_string_lossy().into_owned();
        assert_eq!(s.checked().await, (moved, "ok".into(), None));
        s.rebuild_until(&pack, "done").await;
    }

    #[tokio::test]
    async fn a_changed_original_is_used_only_once_named_and_the_next_build_reads_it() {
        let s = setup().await;
        let (original, pack) = s.import_copy().await;
        let at = original.path.to_string_lossy().into_owned();
        let before = sha256(&original.path);
        changed_package(&original.path);
        let after = sha256(&original.path);
        assert_ne!(before, after);

        assert_eq!(
            s.checked().await,
            (at.clone(), "changed".into(), Some(after.clone()))
        );
        assert_eq!(
            s.engine.rebuild_pack("lib", &pack).await.unwrap_err(),
            CHANGED
        );

        let differs = s
            .locate(&original.path, &original.path, None)
            .await
            .unwrap();
        assert_eq!(
            differs,
            Located::Differs {
                path: at.clone(),
                sha256: after.clone(),
            }
        );
        let error = s
            .locate(&original.path, &original.path, Some(&before))
            .await
            .unwrap_err();
        assert!(
            error.ends_with("changed since it was checked: locate it again"),
            "{error}"
        );
        assert_eq!(
            s.strings("SELECT original_sha256 FROM imports").await,
            [before]
        );

        // Use this file
        let used = s
            .locate(&original.path, &original.path, Some(&after))
            .await
            .unwrap();
        assert_eq!(used, Located::Located);
        assert_eq!(s.checked().await, (at, "ok".into(), None));
        assert_eq!(
            s.strings("SELECT original_sha256 FROM imports").await,
            [after]
        );

        assert!(!s.live().join(ADDED).exists());
        s.rebuild_until(&pack, "done").await;
        assert_eq!(std::fs::read(s.live().join(ADDED)).unwrap(), b"added");
        let manifest = s
            .strings(
                "SELECT o.path FROM import_outputs o JOIN import_packs p
                 ON p.id = o.pack_id AND o.build_id = p.current_build",
            )
            .await;
        assert!(
            manifest.contains(&format!("{TARGET}/{ADDED}")),
            "{manifest:?}"
        );
    }

    #[tokio::test]
    async fn use_this_file_once_rebuilds_every_pack_of_the_original_from_it() {
        let s = setup().await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.unitypackage");
        std::fs::copy(fixture("synthetic.unitypackage"), &path).unwrap();
        // Two imports of the one original, at two targets: one row in the Imports view
        let targets = [TARGET, "Pub/Second"];
        let mut packs = Vec::new();
        for target in targets {
            let mut request = s.golden_request();
            request.original_path = path.to_string_lossy().into_owned();
            request.target = target.into();
            let run = s.engine.start_import(request).await.unwrap();
            s.wait_for(&run.id, "done").await;
            packs.push(run.packs[0].pack_id.clone());
        }
        let path = path.canonicalize().unwrap();
        changed_package(&path);
        let after = sha256(&path);
        let imports = s.engine.check_originals("lib").await.unwrap();
        let states: Vec<_> = imports
            .iter()
            .map(|i| (i.original_state.as_str(), i.changed_sha256.as_deref()))
            .collect();
        assert_eq!(states, [("changed", Some(after.as_str())); 2]);

        // One Use this file names the new original for both imports
        let used = s.locate(&path, &path, Some(&after)).await.unwrap();
        assert_eq!(used, Located::Located);
        assert_eq!(
            s.strings("SELECT original_sha256 || ' ' || original_state FROM imports")
                .await,
            [format!("{after} ok"), format!("{after} ok")]
        );

        for (target, pack) in targets.iter().zip(&packs) {
            let root = s.dir.path().join(target);
            assert!(!root.join(ADDED).exists());
            s.rebuild_until(pack, "done").await;
            assert_eq!(
                std::fs::read(root.join(ADDED)).unwrap(),
                b"added",
                "{target}"
            );
        }
    }

    #[tokio::test]
    async fn an_unchanged_original_is_not_hashed_again() {
        let s = setup().await;
        let (original, _) = s.import_copy().await;
        let real = sha256(&original.path);
        // A hash would find this sha256 wrong: the check reads the stamp only.
        sqlx::query("UPDATE imports SET original_sha256 = 'not its sha256'")
            .execute(&s.lib.pool)
            .await
            .unwrap();
        assert_eq!(s.checked().await.1, "ok");

        // Touched, with the same bytes: hashed, found the same, and its new stamp recorded.
        sqlx::query("UPDATE imports SET original_sha256 = ?")
            .bind(&real)
            .execute(&s.lib.pool)
            .await
            .unwrap();
        let touched = std::time::SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&original.path)
            .unwrap()
            .set_modified(touched)
            .unwrap();
        assert_eq!(s.checked().await.1, "ok");
        let (_, mtime) = crate::import::build::file_stamp(&original.path).unwrap();
        assert_eq!(
            s.count("SELECT original_mtime_ns FROM imports").await,
            mtime,
            "so the next check and the build's own stamp check pass"
        );
    }

    #[tokio::test]
    async fn opening_a_library_checks_its_originals() {
        let s = setup().await;
        let (original, _) = s.import_copy().await;
        std::fs::remove_file(&original.path).unwrap();
        s.engine.library_opened("lib").await;
        for _ in 0..500 {
            let state = s.strings("SELECT original_state FROM imports").await;
            if state == ["missing"] {
                assert!(s.announced());
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the open never checked the original");
    }

    #[tokio::test]
    async fn startup_checks_every_original() {
        let s = setup().await;
        let (original, _) = s.import_copy().await;
        std::fs::remove_file(&original.path).unwrap();

        // The next start of OpenDAM: a new engine over the same libraries.
        let events: Arc<Mutex<Vec<ImportEvent>>> = Default::default();
        let sink = events.clone();
        let (worker, _requests) = mpsc::channel(16);
        let engine = Engine::with_options(
            s.engine.db.clone(),
            Arc::new(move |event| sink.lock().unwrap().push(event)),
            Duration::from_secs(2),
            s.cache.path().to_path_buf(),
            worker,
        );
        tokio::spawn(engine.start());
        for _ in 0..500 {
            if s.strings("SELECT original_state FROM imports").await == ["missing"] {
                let announced = ImportEvent::OriginalsChanged("lib".into());
                assert!(events.lock().unwrap().contains(&announced));
                assert!(!s.announced(), "only the new engine checked");
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the start never checked the original");
    }

    #[tokio::test]
    async fn rebuild_checks_its_original_first() {
        let s = setup().await;
        let (original, pack) = s.import_copy().await;

        // Recorded missing, but back at its path: the Rebuild's own check finds it.
        sqlx::query("UPDATE imports SET original_state = 'missing'")
            .execute(&s.lib.pool)
            .await
            .unwrap();
        s.rebuild_until(&pack, "done").await;
        assert_eq!(
            s.strings("SELECT original_state FROM imports").await,
            ["ok"]
        );

        // Recorded ok, but gone since: refused without the Imports view's check.
        let moved = original.path.with_extension("moved");
        std::fs::rename(&original.path, &moved).unwrap();
        assert_eq!(
            s.engine.rebuild_pack("lib", &pack).await.unwrap_err(),
            MISSING
        );
        assert_eq!(
            s.strings("SELECT original_state FROM imports").await,
            ["missing"]
        );
    }

    #[tokio::test]
    async fn a_rebuild_waits_only_for_a_check_of_its_own_original() {
        let s = setup().await;
        let (first, first_pack) = s.import_copy().await;
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other.unitypackage");
        std::fs::copy(fixture("synthetic.unitypackage"), &other).unwrap();
        let mut request = s.golden_request();
        request.original_path = other.to_string_lossy().into_owned();
        request.target = "Pub/Other".into();
        let run = s.engine.start_import(request).await.unwrap();
        s.wait_for(&run.id, "done").await;
        let other_pack = run.packs[0].pack_id.clone();

        // A check hashing the first original, as a startup check over a touched multi-GB file does
        let held = s.engine.checking.lock(&first.path.to_string_lossy()).await;
        let other_rebuild = tokio::time::timeout(
            Duration::from_secs(10),
            s.engine.rebuild_pack("lib", &other_pack),
        )
        .await
        .expect("a rebuild of another original's pack doesn't wait for that check");
        s.wait_for(&other_rebuild.unwrap().id, "done").await;
        let first_rebuild = tokio::time::timeout(
            Duration::from_millis(300),
            s.engine.rebuild_pack("lib", &first_pack),
        )
        .await;
        assert!(
            first_rebuild.is_err(),
            "the original's own rebuild waits for its check"
        );
        drop(held);
        let run = s.engine.rebuild_pack("lib", &first_pack).await.unwrap();
        s.wait_for(&run.id, "done").await;
    }

    #[tokio::test]
    async fn locate_original_is_refused_while_a_pack_of_it_is_building() {
        let s = setup_with(false).await;
        let mut request = s.golden_request();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.unitypackage");
        std::fs::copy(fixture("synthetic.unitypackage"), &path).unwrap();
        request.original_path = path.to_string_lossy().into_owned();
        s.engine.start_import(request).await.unwrap();
        let path = path.canonicalize().unwrap();
        let error = s.locate(&path, &path, None).await.unwrap_err();
        assert_eq!(error, BUSY);
    }
}
