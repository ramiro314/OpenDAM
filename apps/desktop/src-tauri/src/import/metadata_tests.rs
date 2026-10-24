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
    use crate::import::metadata::{self, Head};
    use crate::import::place::PlaceStep;
    use crate::import::unitypackage::tests::fixture;

    const CAR_KIT: &str = "3D assets/Fixture Car Kit";
    const CAR_KIT_PAGE: &str = "https://kenney.nl/assets/fixture-car-kit";

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

        async fn store_page(&self) -> Option<String> {
            sqlx::query_scalar("SELECT store_page FROM import_packs")
                .fetch_one(&self.lib.pool)
                .await
                .unwrap()
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
        assert_eq!(
            *calls.lock().unwrap(),
            [CAR_KIT_PAGE, "https://kenney.nl/assets/fixture-tiles"]
        );
        assert_eq!(
            s.strings("SELECT store_page FROM import_packs WHERE preset = 'kenney'")
                .await,
            ["", ""],
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
        assert_eq!(s.store_page().await.as_deref(), Some(CAR_KIT_PAGE));
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
        assert_eq!(s.store_page().await.as_deref(), Some(""));
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
            .set_pack_steps("lib", &pack_id, steps.clone())
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
            .set_pack_steps("lib", &pack_id, steps)
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
}
