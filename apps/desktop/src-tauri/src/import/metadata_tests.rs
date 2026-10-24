//! The metadata step in builds (plan pack-import, Pack metadata, A7): each preset's
//! `datapackage.json` against the owner's scripts' goldens, fields set at import surviving a
//! rebuild, and the Kenney store page lookup with the HTTP answer injected.
//!
//! The tests sit in a `mod tests` block, as in every other file, so source scans that stop there
//! (`structure::tests::metadata_writers`) read none of them.

mod tests {
    use std::sync::{Arc, Mutex};

    use futures_util::FutureExt;

    use crate::import::engine::tests::{setup, setup_with, Setup, TARGET};
    use crate::import::engine::*;
    use crate::import::metadata::{self, Head, StorePage};
    use crate::import::place::PlaceStep;
    use crate::import::unitypackage::tests::fixture;

    const CAR_KIT: &str = "3D assets/Fixture Car Kit";
    const CAR_KIT_PAGE: &str = "https://kenney.nl/assets/fixture-car-kit";
    const TILES_PAGE: &str = "https://kenney.nl/assets/fixture-tiles";

    /// The answer kept for a lookup of `urls` that found `page`.
    fn answer(urls: &[&str], page: Option<&str>) -> StorePage {
        StorePage {
            urls: urls.iter().map(|u| u.to_string()).collect(),
            page: page.map(str::to_string),
        }
    }

    /// The golden `expected/<name>`.
    fn golden(name: &str) -> String {
        std::fs::read_to_string(fixture("expected").join(name)).unwrap()
    }

    /// The URLs of the HEAD requests made, each answered 200 when it is `found`, else 404.
    fn kenney_nl(s: &Setup, found: &'static str) -> Arc<Mutex<Vec<String>>> {
        let calls: Arc<Mutex<Vec<String>>> = Default::default();
        let seen = calls.clone();
        let head: Head = Arc::new(move |url: String| {
            let status = if url == found { 200 } else { 404 };
            seen.lock().unwrap().push(url);
            async move { Ok(status) }.boxed()
        });
        s.engine.hooks().store_pages(head);
        calls
    }

    impl Setup {
        /// The `datapackage.json` built at `target`.
        fn package(&self, target: &str) -> String {
            std::fs::read_to_string(self.dir.path().join(target).join(metadata::FILE)).unwrap()
        }

        /// An import of the Kenney fixture's pack `pack` (`<Category>/<Pack>`), at that target.
        fn kenney_request(&self, pack: &str) -> ImportRequest {
            let mut request =
                self.zip_request("kenney.zip", None, Some(&format!("Kenney/{pack}")), pack);
            request.preset = Some("kenney".into());
            request.publisher = "Kenney".into();
            request
        }

        async fn import(&self, request: ImportRequest) -> String {
            let run = self.engine.start_import(request).await.unwrap();
            self.wait_for(&run.id, "done").await;
            run.packs[0].pack_id.clone()
        }

        async fn rebuild_done(&self, pack_id: &str) {
            let run = self.engine.rebuild_pack("lib", pack_id).await.unwrap();
            self.wait_for(&run.id, "done").await;
        }

        /// The kept store page answer.
        async fn store_page(&self) -> Option<StorePage> {
            let kept: Option<String> = sqlx::query_scalar("SELECT store_page FROM import_packs")
                .fetch_one(&self.lib.pool)
                .await
                .unwrap();
            kept.map(|json| serde_json::from_str(&json).unwrap())
        }

        async fn recipe_rev(&self) -> i64 {
            self.count("SELECT recipe_rev FROM import_packs").await
        }
    }

    #[tokio::test]
    async fn each_preset_writes_the_scripts_datapackage_json() {
        let s = setup().await;
        let calls = kenney_nl(&s, "none");
        // Unity: extract and flatten, as the preset does.
        s.import(s.request(vec![PlaceStep::Flatten])).await;
        assert_eq!(s.package(TARGET), golden("synthetic.datapackage.json"));
        // Fab: the URP variant, flatten and hoist Art.
        let variant = Some("Crypt/Crypt_URP.unitypackage");
        s.import(s.zip_request("fab-unity.zip", variant, None, "Fab/Crypt"))
            .await;
        assert_eq!(s.package("Fab/Crypt"), golden("fab-unity.datapackage.json"));
        assert!(calls.lock().unwrap().is_empty(), "only Kenney looks up");

        // Kenney, one pack with a model and one without, neither with a store page.
        for (pack, name) in [
            (CAR_KIT, "kenney-car-kit"),
            ("2D assets/Fixture Tiles", "kenney-tiles"),
        ] {
            s.import(s.kenney_request(pack)).await;
            assert_eq!(s.package(pack), golden(&format!("{name}.datapackage.json")));
        }
        assert_eq!(*calls.lock().unwrap(), [CAR_KIT_PAGE, TILES_PAGE]);
        assert_eq!(
            s.strings(
                "SELECT store_page FROM import_packs WHERE preset = 'kenney' ORDER BY target"
            )
            .await,
            [
                answer(&[TILES_PAGE], None).to_json(),
                answer(&[CAR_KIT_PAGE], None).to_json()
            ],
            "a miss is kept too"
        );

        // The file is the build's own, generated from no entry of the original.
        assert_eq!(
            s.strings(&format!(
                "SELECT o.origin || ' ' || o.source_path FROM import_outputs o
                 JOIN import_packs p ON p.id = o.pack_id AND o.build_id = p.current_build
                 WHERE o.path = '{TARGET}/datapackage.json'"
            ))
            .await,
            ["generated []"]
        );
        // Indexed, it makes its folder a pack, which the structure pass reads it into.
        assert_eq!(
            s.count(&format!(
                "SELECT COUNT(*) FROM folders WHERE path = '{TARGET}' AND is_pack = 1
                 AND json_extract(pack_json, '$.package') IS NOT NULL"
            ))
            .await,
            1
        );
    }

    #[tokio::test]
    async fn fields_set_at_import_survive_a_rebuild() {
        let s = setup().await;
        let mut request = s.zip_request(
            "fab-unity.zip",
            Some("Crypt/Crypt_URP.unitypackage"),
            None,
            "Stylarts/Crypt",
        );
        request.publisher = "Leartes Studios".into();
        request.version = Some(" 1.2 ".into());
        let pack_id = s.import(request).await;
        let built = s.package("Stylarts/Crypt");
        let package: serde_json::Value = serde_json::from_str(&built).unwrap();
        assert_eq!(
            package["contributors"],
            serde_json::json!([{"title": "Leartes Studios", "roles": ["publisher"]}]),
            "the publisher set, not the folder above the pack"
        );
        assert_eq!(package["version"], "1.2");
        let file = s.dir.path().join("Stylarts/Crypt").join(metadata::FILE);
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();

        s.rebuild_done(&pack_id).await;
        assert_eq!(s.package("Stylarts/Crypt"), built);
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            mtime,
            "an unchanged datapackage.json keeps its mtime"
        );
    }

    #[tokio::test]
    async fn a_found_store_page_is_kept_outside_the_recipe_and_reused() {
        let s = setup_with(false).await;
        let calls = kenney_nl(&s, CAR_KIT_PAGE);
        let run = s
            .engine
            .start_import(s.kenney_request(CAR_KIT))
            .await
            .unwrap();
        let pack_id = run.packs[0].pack_id.clone();
        assert_eq!(
            s.engine.clear_store_page("lib", &pack_id).await,
            Err(BUSY.to_string())
        );
        // A second run queued meanwhile, at the same recipe_rev.
        sqlx::query(
            "INSERT INTO import_runs (id, kind, packs, status, created_at, owner_machine,
             owner_boot, owner_pid, owner_started, heartbeat_at)
             SELECT 'second', 'rebuild', packs, 'queued', created_at + 1, owner_machine,
             owner_boot, owner_pid, owner_started, heartbeat_at FROM import_runs",
        )
        .execute(&s.lib.pool)
        .await
        .unwrap();
        tokio::spawn(s.engine.clone().start());
        s.wait_for(&run.id, "done").await;
        s.wait_for("second", "done").await;

        let with_source = golden("kenney-car-kit.datapackage.json").replace(
            "  \"homepage\"",
            &format!(
                "  \"sources\": [\n    {{\n      \"title\": \"Kenney\",\n      \"path\": \
                 \"{CAR_KIT_PAGE}\"\n    }}\n  ],\n  \"homepage\""
            ),
        );
        assert_eq!(s.package(CAR_KIT), with_source);
        assert_eq!(
            *calls.lock().unwrap(),
            [CAR_KIT_PAGE],
            "the second run asked nothing"
        );
        assert_eq!(
            s.store_page().await,
            Some(answer(&[CAR_KIT_PAGE], Some(CAR_KIT_PAGE)))
        );
        assert_eq!(s.recipe_rev().await, 0, "the answer is no step change");

        s.rebuild_done(&pack_id).await;
        assert_eq!(s.package(CAR_KIT), with_source);
        assert_eq!(calls.lock().unwrap().len(), 1, "a rebuild makes no request");

        s.engine.clear_store_page("lib", &pack_id).await.unwrap();
        assert_eq!(s.store_page().await, None);
        s.rebuild_done(&pack_id).await;
        assert_eq!(*calls.lock().unwrap(), [CAR_KIT_PAGE, CAR_KIT_PAGE]);
        assert_eq!(s.package(CAR_KIT), with_source);
        assert_eq!(s.recipe_rev().await, 0);
    }

    #[tokio::test]
    async fn no_request_is_made_with_the_lookup_off_or_unanswered() {
        let s = setup().await;
        let calls = kenney_nl(&s, CAR_KIT_PAGE);
        let mut request = s.kenney_request(CAR_KIT);
        request.lookup_store_page = Some(false);
        let pack_id = s.import(request).await;
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(
            s.package(CAR_KIT),
            golden("kenney-car-kit.datapackage.json")
        );
        assert_eq!(
            s.count(
                "SELECT COUNT(*) FROM import_steps WHERE kind = 'metadata'
                 AND json_extract(options, '$.lookup_store_page') = 0"
            )
            .await,
            1
        );
        s.rebuild_done(&pack_id).await;
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(s.store_page().await, None);

        // A lookup that gets no answer (the network is down) keeps nothing, so the next build
        // asks again.
        let tiles = "2D assets/Fixture Tiles";
        let s = setup().await;
        let pack_id = s.import(s.kenney_request(tiles)).await;
        assert_eq!(s.store_page().await, None);
        assert_eq!(s.package(tiles), golden("kenney-tiles.datapackage.json"));
        let calls = kenney_nl(&s, "none");
        s.rebuild_done(&pack_id).await;
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(s.store_page().await, Some(answer(&[TILES_PAGE], None)));
    }

    #[tokio::test]
    async fn names_with_no_slug_ask_for_no_page() {
        let s = setup().await;
        let calls = kenney_nl(&s, "https://kenney.nl/assets/");
        let mut request = s.kenney_request(CAR_KIT);
        request.target = "3D assets/車".into();
        request.title = Some("車キット".into());
        s.import(request).await;
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(s.store_page().await, None);
        let package = s.package("3D assets/車");
        let package: serde_json::Value = serde_json::from_str(&package).unwrap();
        assert_eq!(package["title"], "車キット");
        assert!(package.get("sources").is_none(), "{package}");
    }

    #[tokio::test]
    async fn a_title_set_later_looks_the_page_up_again() {
        let s = setup().await;
        // The header's title has no page.
        let calls = kenney_nl(&s, "none");
        let pack_id = s.import(s.kenney_request(CAR_KIT)).await;
        assert_eq!(s.store_page().await, Some(answer(&[CAR_KIT_PAGE], None)));

        // The right title, whose page exists.
        let car_kit = "https://kenney.nl/assets/car-kit";
        let calls_after = kenney_nl(&s, car_kit);
        let mut steps = s.engine.pack_steps("lib", &pack_id).await.unwrap().steps;
        let at = steps
            .iter()
            .position(|step| step.kind == "metadata")
            .unwrap();
        steps[at].options["title"] = "Car Kit".into();
        s.engine
            .set_pack_steps("lib", &pack_id, steps.clone(), None)
            .await
            .unwrap();
        s.rebuild_done(&pack_id).await;
        assert_eq!(*calls.lock().unwrap(), [CAR_KIT_PAGE]);
        assert_eq!(
            *calls_after.lock().unwrap(),
            [car_kit],
            "the new slug first"
        );
        assert_eq!(
            s.store_page().await,
            Some(answer(&[car_kit, CAR_KIT_PAGE], Some(car_kit)))
        );
        let package: serde_json::Value = serde_json::from_str(&s.package(CAR_KIT)).unwrap();
        assert_eq!(
            package["sources"],
            serde_json::json!([{"title": "Kenney", "path": car_kit}])
        );

        // Kept for those URLs: a rebuild asks nothing.
        s.rebuild_done(&pack_id).await;
        assert_eq!(calls_after.lock().unwrap().len(), 1);

        // Back to the header's title: the page found for the other title isn't reused.
        steps[at].options["title"] = serde_json::Value::Null;
        s.engine
            .set_pack_steps("lib", &pack_id, steps, None)
            .await
            .unwrap();
        s.rebuild_done(&pack_id).await;
        assert_eq!(*calls_after.lock().unwrap(), [car_kit, CAR_KIT_PAGE]);
        assert_eq!(s.store_page().await, Some(answer(&[CAR_KIT_PAGE], None)));
        assert_eq!(
            s.package(CAR_KIT),
            golden("kenney-car-kit.datapackage.json")
        );
    }

    #[tokio::test]
    async fn the_metadata_step_turned_off_writes_no_file_and_bad_options_are_refused() {
        let s = setup().await;
        let pack_id = s.import(s.request(vec![PlaceStep::Flatten])).await;
        let mut steps = s.engine.pack_steps("lib", &pack_id).await.unwrap().steps;
        let at = steps
            .iter()
            .position(|step| step.kind == "metadata")
            .unwrap();
        steps[at].options["lookup"] = true.into();
        let refused = s
            .engine
            .set_pack_steps("lib", &pack_id, steps.clone(), None)
            .await;
        assert!(
            refused
                .unwrap_err()
                .starts_with("The metadata step's options are invalid"),
            "an unknown option"
        );
        steps[at].options = serde_json::json!({"preset": "unity"});
        steps[at].enabled = false;
        s.engine
            .set_pack_steps("lib", &pack_id, steps, None)
            .await
            .unwrap();
        s.rebuild_done(&pack_id).await;
        assert!(!s.live().join(metadata::FILE).exists());
        assert_eq!(
            s.count(&format!(
                "SELECT COUNT(*) FROM assets WHERE file_path = '{TARGET}/datapackage.json'"
            ))
            .await,
            0,
            "its row was pruned"
        );
    }

    #[tokio::test]
    async fn an_original_s_own_datapackage_json_at_the_pack_root_fails_the_build() {
        use std::io::Write;
        let s = setup().await;
        // Outside the library: a zip whose `Docs/datapackage.json` reaches the pack root once
        // `Docs` is hoisted.
        let originals = tempfile::tempdir().unwrap();
        let original = originals.path().join("own.zip");
        let own: &[u8] = b"{\"name\": \"theirs\"}\n";
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&original).unwrap());
        for (name, data) in [
            ("Own/a.fbx", &b"fbx"[..]),
            ("Own/Docs/datapackage.json", own),
        ] {
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file(name, stored).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        let request = ImportRequest {
            original_path: original.to_string_lossy().into_owned(),
            target: "Own".into(),
            preset: Some("none".into()),
            place: Some(vec![PlaceStep::Flatten]),
            ..s.request(vec![])
        };
        let pack_id = s.import(request).await;
        let live = s.dir.path().join("Own");
        let ours = std::fs::read_to_string(live.join(metadata::FILE)).unwrap();
        assert_eq!(
            std::fs::read(live.join("Docs/datapackage.json")).unwrap(),
            own
        );
        let build = s.pack().await.1;

        let mut steps = s.engine.pack_steps("lib", &pack_id).await.unwrap().steps;
        let at = steps
            .iter()
            .position(|step| step.kind == "metadata")
            .unwrap();
        steps.insert(
            at,
            Step {
                kind: "hoist".into(),
                enabled: true,
                options: serde_json::json!({"folder": "Docs"}),
            },
        );
        s.engine
            .set_pack_steps("lib", &pack_id, steps, None)
            .await
            .unwrap();
        let run = s.engine.rebuild_pack("lib", &pack_id).await.unwrap();
        let run = s.wait_for(&run.id, "failed").await;
        assert_eq!(
            run.error.as_deref(),
            Some(
                "The original holds its own datapackage.json where the metadata step writes \
                 datapackage.json: turn the step off to keep it"
            )
        );
        // The live pack is as the last build left it, its own file not overwritten.
        assert_eq!(
            crate::import::unitypackage::tests::on_disk(&live),
            ["Docs/datapackage.json", "a.fbx", "datapackage.json"]
        );
        assert_eq!(
            std::fs::read(live.join("Docs/datapackage.json")).unwrap(),
            own
        );
        assert_eq!(
            std::fs::read_to_string(live.join(metadata::FILE)).unwrap(),
            ours
        );
        assert_eq!(s.pack().await, ("built".into(), build, None));
        for tag in ["import", "prev", "failed"] {
            let left = crate::fs_swap::leftovers(&live, tag).unwrap();
            assert!(left.is_empty(), "the staging folder is gone: {left:?}");
        }
    }
}
