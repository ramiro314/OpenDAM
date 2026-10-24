//! The `unity_glb` step in builds (plan pack-import, Unity conversion, A8 and A12): the GLB
//! beside each model, its cache across rebuilds, Blender's memory cap, and the step without
//! Blender. The A8 cases run Blender, from `OPENDAM_BLENDER`, and skip without it outside CI.
//!
//! The tests sit in a `mod tests` block, as in every other file, so source scans that stop there
//! (`structure::tests::metadata_writers`) read none of them.

mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use serde_json::{json, Value};

    use crate::export::blender::test_blender;
    use crate::import::engine::tests::{setup, Setup, TARGET};
    use crate::import::engine::*;
    use crate::import::unitypackage::tests::fixture;

    /// The synthetic package's prefix to every source path.
    const WRAP: &str = "Assets/Pack/Built-In/Built-In_Pack/Pack";

    fn executable(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    impl Setup {
        /// Points Settings at `program` as Blender.
        async fn set_blender(&self, program: &Path) {
            crate::export::settings::update(
                &self.engine.db.master_pool,
                json!({ "blenderPath": program }),
            )
            .await
            .unwrap();
        }

        /// Points Settings at the test Blender, through a wrapper that logs each conversion it
        /// runs, and returns the log. `None` when there's no test Blender.
        async fn logged_blender(&self) -> Option<PathBuf> {
            let real = test_blender()?;
            let bin = self.cache.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let log = bin.join("runs.log");
            let wrapper = bin.join("blender");
            executable(
                &wrapper,
                &format!(
                    "[ \"$1\" = -b ] && echo run >> '{}'\nexec '{}' \"$@\"",
                    log.display(),
                    real.display()
                ),
            );
            self.set_blender(&wrapper).await;
            Some(log)
        }

        async fn edit(&self, pack_id: &str, edit: impl FnOnce(&mut Vec<Step>)) {
            let mut steps = self.engine.pack_steps("lib", pack_id).await.unwrap().steps;
            edit(&mut steps);
            self.engine
                .set_pack_steps("lib", pack_id, steps)
                .await
                .unwrap();
        }

        async fn rebuilt(&self, pack_id: &str) {
            let run = self.engine.rebuild_pack("lib", pack_id).await.unwrap();
            self.wait_for(&run.id, "done").await;
        }

        /// The current build's generated files, as `(path, source path)`.
        async fn generated(&self) -> Vec<(String, Vec<String>)> {
            let rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT o.path, o.source_path FROM import_outputs o JOIN import_packs p
                 ON p.id = o.pack_id AND o.build_id = p.current_build
                 WHERE o.origin = 'generated' ORDER BY o.path",
            )
            .fetch_all(&self.lib.pool)
            .await
            .unwrap();
            rows.into_iter()
                .map(|(path, source)| (path, serde_json::from_str(&source).unwrap()))
                .collect()
        }

        /// No work folder is left in the cache.
        fn no_work_left(&self) {
            let work = self.cache.path().join("work");
            let left: Vec<_> = std::fs::read_dir(&work)
                .map(|d| d.map(|e| e.unwrap().file_name()).collect())
                .unwrap_or_default();
            assert!(left.is_empty(), "{left:?}");
        }
    }

    fn runs(log: &Path) -> usize {
        std::fs::read_to_string(log)
            .map(|t| t.lines().count())
            .unwrap_or(0)
    }

    /// A GLB's JSON chunk and binary chunk.
    fn glb(path: &Path) -> (Value, Vec<u8>) {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[..4], b"glTF");
        let chunk = |at: usize| {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (&bytes[at + 8..at + 8 + len], at + 8 + len)
        };
        let (json, next) = chunk(12);
        let bin = if next < bytes.len() {
            chunk(next).0.to_vec()
        } else {
            Vec::new()
        };
        (serde_json::from_slice(json).unwrap(), bin)
    }

    /// The colour at the first pixel of each base color texture the GLB's materials use.
    fn base_colors(path: &Path) -> Vec<[u8; 3]> {
        let (json, bin) = glb(path);
        let materials = json["materials"].as_array().cloned().unwrap_or_default();
        materials
            .iter()
            .filter_map(|m| m["pbrMetallicRoughness"]["baseColorTexture"]["index"].as_u64())
            .map(|texture| {
                let image = &json["images"][json["textures"][texture as usize]["source"]
                    .as_u64()
                    .unwrap() as usize];
                let view = &json["bufferViews"][image["bufferView"].as_u64().unwrap() as usize];
                let start = view["byteOffset"].as_u64().unwrap_or(0) as usize;
                let len = view["byteLength"].as_u64().unwrap() as usize;
                let decoded = image::load_from_memory(&bin[start..start + len])
                    .unwrap()
                    .to_rgb8();
                decoded.get_pixel(0, 0).0
            })
            .collect()
    }

    /// The fixture's `T_Log_D.png` is (110, 80, 50), through a JPEG.
    fn is_log_brown(color: [u8; 3]) -> bool {
        color
            .iter()
            .zip([110u8, 80, 50])
            .all(|(got, want)| got.abs_diff(want) <= 4)
    }

    fn sha(path: &Path) -> String {
        crate::export::hashes::sha256_file(path).unwrap()
    }

    fn texture_source() -> Vec<String> {
        vec![format!("{WRAP}/Art/Textures/T_Log_D.png")]
    }

    fn hoist(folder: &str) -> Step {
        Step {
            kind: "hoist".into(),
            enabled: true,
            options: json!({ "folder": folder }),
        }
    }

    /// Puts `step` before the `unity_glb` step.
    fn insert(steps: &mut Vec<Step>, step: Step) {
        let at = steps.iter().position(|s| s.kind == "unity_glb").unwrap();
        steps.insert(at, step);
    }

    /// A8: `SM_Log.glb` carries the fixture PNG; moving files between folders reuses it with no
    /// Blender run; reordering two same-named PNGs, renaming the PNG and excluding it each run
    /// Blender again.
    #[tokio::test]
    async fn sm_log_gets_a_textured_glb_reconverted_only_when_its_inputs_change() {
        let s = setup().await;
        let Some(log) = s.logged_blender().await else {
            return;
        };
        let run = s
            .engine
            .start_import(s.request(vec![PlaceStep::Flatten]))
            .await
            .unwrap();
        s.wait_for(&run.id, "done").await;
        let pack_id = run.packs[0].pack_id.clone();
        let steps = s.engine.pack_steps("lib", &pack_id).await.unwrap();
        let convert = steps.steps.iter().find(|s| s.kind == "unity_glb").unwrap();
        assert!(convert.enabled, "on, since Blender is there");
        assert_eq!(convert.options, json!({"max_texture": 1024}));
        assert!(steps.unavailable.is_empty(), "{:?}", steps.unavailable);

        let model = vec![format!("{WRAP}/Art/Models/SM_Log.fbx")];
        let glb_at = |rel: &str| s.live().join(rel);
        assert_eq!(runs(&log), 1);
        assert_eq!(
            s.generated().await,
            [(format!("{TARGET}/Art/Models/SM_Log.glb"), model.clone())]
        );
        let first = glb_at("Art/Models/SM_Log.glb");
        let colors = base_colors(&first);
        assert!(
            colors.len() == 1 && is_log_brown(colors[0]),
            "the GLB carries T_Log_D.png: {colors:?}"
        );
        let (bytes, mtime) = (
            sha(&first),
            std::fs::metadata(&first).unwrap().modified().unwrap(),
        );
        // Indexed, as the main item of the model's format group.
        let grouped: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM assets g JOIN assets m ON m.group_id = g.id
             WHERE g.file_path = ? AND m.file_path = ?",
        )
        .bind(format!("{TARGET}/Art/Models/SM_Log.glb"))
        .bind(format!("{TARGET}/Art/Models/SM_Log.fbx"))
        .fetch_one(&s.lib.pool)
        .await
        .unwrap();
        assert_eq!(grouped, 1);
        s.no_work_left();

        // Hoisting Houses moves files and keeps every name and the T_Rock.png order: the cached
        // GLB comes back, with its bytes and its mtime.
        s.edit(&pack_id, |steps| insert(steps, hoist("Houses")))
            .await;
        s.rebuilt(&pack_id).await;
        assert_eq!(runs(&log), 1, "no Blender run");
        let hoisted = glb_at("Art/Models/SM_Log.glb");
        assert_eq!(sha(&hoisted), bytes);
        assert_eq!(
            std::fs::metadata(&hoisted).unwrap().modified().unwrap(),
            mtime
        );
        assert!(s
            .live()
            .join("Built-In/Built-In_Pack/Pack/Art/T_Rock.png")
            .is_file());

        // Hoisting Art too puts the nested package's T_Rock.png first by path: Blender again.
        s.edit(&pack_id, |steps| insert(steps, hoist("Art"))).await;
        s.rebuilt(&pack_id).await;
        assert_eq!(runs(&log), 2, "a reorder of same-named PNGs reconverts");
        assert_eq!(
            s.generated().await,
            [(format!("{TARGET}/Models/SM_Log.glb"), model.clone())]
        );
        assert!(is_log_brown(base_colors(&glb_at("Models/SM_Log.glb"))[0]));

        // A rename of the PNG reconverts; the material still finds it by GUID.
        s.edit(&pack_id, |steps| {
            let rename = PlaceStep::Rename {
                source: texture_source(),
                name: "T_Log_Base.png".into(),
            };
            insert(steps, step(&rename));
        })
        .await;
        s.rebuilt(&pack_id).await;
        assert_eq!(runs(&log), 3, "a rename reconverts");
        assert!(s.live().join("Textures/T_Log_Base.png").is_file());
        assert!(is_log_brown(base_colors(&glb_at("Models/SM_Log.glb"))[0]));

        // An exclude of the PNG reconverts, and the GLB carries no texture.
        s.edit(&pack_id, |steps| {
            let exclude = PlaceStep::Exclude {
                source: texture_source(),
            };
            insert(steps, step(&exclude));
        })
        .await;
        s.rebuilt(&pack_id).await;
        assert_eq!(runs(&log), 4, "an exclude reconverts");
        assert!(!s.live().join("Textures/T_Log_Base.png").exists());
        assert_eq!(
            base_colors(&glb_at("Models/SM_Log.glb")),
            Vec::<[u8; 3]>::new()
        );
        s.no_work_left();
    }

    fn step(place: &PlaceStep) -> Step {
        let record = crate::import::records::StepRecord::place(place);
        Step {
            kind: record.kind,
            enabled: true,
            options: record.options,
        }
    }

    fn sm_log_fbx() -> Vec<u8> {
        std::fs::read(fixture(
            "../game-export/library/Unity Moss/Models/SM_Log.fbx",
        ))
        .unwrap()
    }

    /// A zip original at `dir/name` holding `entries`.
    fn zip_of(dir: &Path, name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join(name);
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        for (name, bytes) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    impl Setup {
        /// Imports the zip original at `path` with its preset's steps plus `unity_glb`, and
        /// returns the pack's id. The preset has no `unity_glb`, so the first build runs none.
        async fn import_zip_converted(&self, path: &Path) -> String {
            let request = ImportRequest {
                original_path: path.to_string_lossy().into_owned(),
                ..self.request(vec![])
            };
            let run = self.engine.start_import(request).await.unwrap();
            self.wait_for(&run.id, "done").await;
            let pack_id = run.packs[0].pack_id.clone();
            self.edit(&pack_id, |steps| {
                steps.push(Step {
                    kind: "unity_glb".into(),
                    enabled: true,
                    options: json!({}),
                })
            })
            .await;
            self.rebuilt(&pack_id).await;
            pack_id
        }
    }

    /// The names of a GLB's nodes.
    fn node_names(path: &Path) -> Vec<String> {
        let (json, _) = glb(path);
        json["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["name"].as_str().map(str::to_string))
            .collect()
    }

    /// A8: one `Crate.glb`, from the FBX, and none beside a model whose folder ships its GLB.
    #[tokio::test]
    async fn a_folder_with_crate_fbx_and_crate_obj_gets_one_glb_from_the_fbx() {
        let s = setup().await;
        let Some(log) = s.logged_blender().await else {
            return;
        };
        let originals = tempfile::tempdir().unwrap();
        let fbx = sm_log_fbx();
        let obj = b"o FromObj\nv 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n";
        let zip = zip_of(
            originals.path(),
            "crates.zip",
            &[
                ("Crate/Crate.fbx", &fbx),
                ("Crate/Crate.obj", obj),
                ("Shipped/Rock.fbx", &fbx),
                ("Shipped/Rock.glb", b"glTF shipped with the pack"),
            ],
        );
        s.import_zip_converted(&zip).await;

        assert_eq!(runs(&log), 1);
        assert_eq!(
            s.generated().await,
            [(
                format!("{TARGET}/Crate/Crate.glb"),
                vec!["Crate/Crate.fbx".to_string()]
            )]
        );
        let (json, _) = glb(&s.live().join("Crate/Crate.glb"));
        let nodes: Vec<&str> = json["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["name"].as_str())
            .collect();
        assert!(
            !nodes.is_empty() && !nodes.contains(&"FromObj"),
            "{nodes:?}"
        );
        assert_eq!(
            std::fs::read(s.live().join("Shipped/Rock.glb")).unwrap(),
            b"glTF shipped with the pack"
        );
    }

    /// An ASCII FBX, which Blender can't import, converts through a binary copy `assimp` makes.
    /// Skips without `assimp` on `PATH`, as in CI.
    #[tokio::test]
    async fn an_ascii_fbx_converts_through_assimp() {
        let s = setup().await;
        let Some(log) = s.logged_blender().await else {
            return;
        };
        let originals = tempfile::tempdir().unwrap();
        let binary = originals.path().join("binary.fbx");
        std::fs::write(&binary, sm_log_fbx()).unwrap();
        let ascii = originals.path().join("ascii.fbx");
        let made = std::process::Command::new("assimp")
            .arg("export")
            .arg(&binary)
            .arg(&ascii)
            .arg("-ffbxa")
            .output();
        if !made.is_ok_and(|o| o.status.success()) {
            eprintln!("Skipping: assimp isn't on PATH to write an ASCII FBX");
            return;
        }
        let text = std::fs::read(&ascii).unwrap();
        assert!(text.starts_with(b"; FBX"), "an ASCII FBX");
        let zip = zip_of(originals.path(), "ascii.zip", &[("Logs/Log.fbx", &text)]);
        s.import_zip_converted(&zip).await;

        assert_eq!(runs(&log), 1);
        assert_eq!(
            s.generated().await,
            [(
                format!("{TARGET}/Logs/Log.glb"),
                vec!["Logs/Log.fbx".to_string()]
            )]
        );
        assert!(!node_names(&s.live().join("Logs/Log.glb")).is_empty());
        assert_eq!(std::fs::read(s.live().join("Logs/Log.fbx")).unwrap(), text);
        s.no_work_left();
    }

    /// A12: Blender killed with status 137 under the cap fails its step, naming the cap, and the
    /// build changes nothing.
    #[tokio::test]
    async fn blender_killed_by_the_memory_cap_fails_the_step() {
        let s = setup().await;
        let bin = s.cache.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let blender = bin.join("blender");
        executable(
            &blender,
            "[ \"$1\" = --version ] && { echo 'Blender 5.2.2'; exit 0; }\nexit 137",
        );
        s.set_blender(&blender).await;
        crate::import::settings::update(
            &s.engine.db.master_pool,
            json!({"blenderMemoryMax": "3G"}),
        )
        .await
        .unwrap();
        // A user manager that accepts every scope and runs its command, as on a desktop.
        executable(&bin.join("systemctl"), "exit 0");
        executable(
            &bin.join("systemd-run"),
            "while [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"",
        );
        s.engine.set_systemd(crate::capped::Systemd {
            run: bin.join("systemd-run"),
            ctl: bin.join("systemctl"),
        });

        let run = s
            .engine
            .start_import(s.request(vec![PlaceStep::Flatten]))
            .await
            .unwrap();
        let failed = s.wait_for(&run.id, "failed").await;
        assert_eq!(
            failed.error.as_deref(),
            Some("Converting to GLB failed: Blender was killed by the memory cap (3G)")
        );
        assert!(!s.live().exists());
        s.no_leftovers();
        s.no_work_left();
    }

    /// Without Blender an import leaves the step off and says why, and a build with it on fails
    /// with that reason.
    #[tokio::test]
    async fn without_blender_the_step_is_off_with_the_reason() {
        let s = setup().await;
        let run = s
            .engine
            .start_import(s.request(vec![PlaceStep::Flatten]))
            .await
            .unwrap();
        s.wait_for(&run.id, "done").await;
        let pack_id = run.packs[0].pack_id.clone();
        let steps = s.engine.pack_steps("lib", &pack_id).await.unwrap();
        let convert = steps.steps.iter().find(|s| s.kind == "unity_glb").unwrap();
        assert!(!convert.enabled);
        let why =
            "Converting to GLB needs Blender 5.2: No Blender at /nonexistent/opendam-test/blender";
        assert_eq!(
            steps.unavailable.get("unity_glb").map(String::as_str),
            Some(why)
        );
        assert_eq!(s.generated().await, []);

        s.edit(&pack_id, |steps| {
            for step in steps.iter_mut().filter(|s| s.kind == "unity_glb") {
                step.enabled = true;
            }
        })
        .await;
        let run = s.engine.rebuild_pack("lib", &pack_id).await.unwrap();
        let failed = s.wait_for(&run.id, "failed").await;
        assert_eq!(failed.error.as_deref(), Some(why));
        s.no_leftovers();
    }
}
