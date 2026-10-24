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

    use std::sync::{Arc, LazyLock, Mutex};

    use crate::export::blender::test_blender;
    use crate::import::engine::tests::{setup, Setup, TARGET};
    use crate::import::engine::*;
    use crate::import::unitypackage::tests::fixture;
    use crate::preview::WorkerRequest;

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
                .set_pack_steps("lib", pack_id, steps, None)
                .await
                .unwrap();
        }

        async fn rebuilt(&self, pack_id: &str) {
            let run = self.engine.rebuild_pack("lib", pack_id).await.unwrap();
            self.wait_for(&run.id, "done").await;
        }

        /// The current build's generated GLBs (generated files with a source path, unlike the
        /// metadata step's `datapackage.json`), as `(path, source path)`.
        async fn generated(&self) -> Vec<(String, Vec<String>)> {
            let rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT o.path, o.source_path FROM import_outputs o JOIN import_packs p
                 ON p.id = o.pack_id AND o.build_id = p.current_build
                 WHERE o.origin = 'generated' AND o.source_path != '[]' ORDER BY o.path",
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
            let (pack_id, _) = self.import_zip_then_convert(path, "done").await;
            pack_id
        }

        /// `import_zip_converted`, the rebuild ending as `status`. Returns the pack's id and the
        /// rebuild's run.
        async fn import_zip_then_convert(
            &self,
            path: &Path,
            status: &str,
        ) -> (String, crate::import::records::RunSummary) {
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
            let run = self.engine.rebuild_pack("lib", &pack_id).await.unwrap();
            let run = self.wait_for(&run.id, status).await;
            (pack_id, run)
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
        let pack_id = s.import_zip_converted(&zip).await;

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

        // A plain rebuild takes the GLB from the cache; `force` converts it again regardless.
        s.rebuilt(&pack_id).await;
        assert_eq!(runs(&log), 1, "cached");
        let set_force = |force: bool| {
            move |steps: &mut Vec<Step>| {
                let convert = steps.iter_mut().find(|s| s.kind == "unity_glb").unwrap();
                convert.options = json!({ "force": force });
            }
        };
        s.edit(&pack_id, set_force(true)).await;
        s.rebuilt(&pack_id).await;
        assert_eq!(runs(&log), 2, "force runs Blender despite the cache");
        s.edit(&pack_id, set_force(false)).await;
        s.rebuilt(&pack_id).await;
        assert_eq!(runs(&log), 2, "cached again");
    }

    /// A 4 x 4 opaque PNG of one colour.
    fn png(rgb: [u8; 3]) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        img.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    fn near(color: [u8; 3], want: [u8; 3]) -> bool {
        color
            .iter()
            .zip(want)
            .all(|(got, want)| got.abs_diff(want) <= 4)
    }

    /// A material library and a texture the OBJ importer finds by path, relative to the model,
    /// are found by their names in the pack instead, so a hoist that moves only the model away
    /// from them reuses the cached GLB, and that GLB is what a fresh conversion in the new layout
    /// makes.
    #[tokio::test]
    async fn an_importer_s_mtl_and_texture_are_found_by_name_whatever_the_layout() {
        let s = setup().await;
        let Some(log) = s.logged_blender().await else {
            return;
        };
        let blue = [40, 120, 200];
        let originals = tempfile::tempdir().unwrap();
        // No .mat, prefab or name rule matches "Wood" or "Barrel": the importer's material stays.
        let obj =
            b"mtllib ../Materials/Wood.mtl\no Barrel\nv 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 0 1\n\
                    usemtl Wood\nf 1/1 2/2 3/3\n";
        let mtl = b"newmtl Wood\nKd 1 1 1\nmap_Kd ../Textures/wood_planks.png\n";
        let missing = b"newmtl Gone\nKd 1 1 1\nmap_Kd ../Textures/not_in_the_pack.png\n";
        let obj_gone =
            b"mtllib Stump.mtl\no Stump\nv 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 0 1\n\
                         usemtl Gone\nf 1/1 2/2 3/3\n";
        let zip = zip_of(
            originals.path(),
            "barrel.zip",
            &[
                ("Models/Barrel.obj", obj),
                ("Materials/Wood.mtl", mtl),
                ("Models/Stump.obj", obj_gone),
                ("Models/Stump.mtl", missing),
                ("Textures/wood_planks.png", &png(blue)),
            ],
        );
        let pack_id = s.import_zip_converted(&zip).await;
        assert_eq!(runs(&log), 1);
        let colors = base_colors(&s.live().join("Models/Barrel.glb"));
        assert!(colors.len() == 1 && near(colors[0], blue), "{colors:?}");
        // A texture no file of the pack is named after is dropped, whatever the importer found.
        assert_eq!(
            base_colors(&s.live().join("Models/Stump.glb")),
            Vec::<[u8; 3]>::new()
        );
        let cached = sha(&s.live().join("Models/Barrel.glb"));

        // Hoisting Models takes Barrel.obj out of reach of `../Materials/`, and Stump.mtl out of
        // reach of `../Textures/`: still the same GLBs.
        s.edit(&pack_id, |steps| insert(steps, hoist("Models")))
            .await;
        s.rebuilt(&pack_id).await;
        assert_eq!(runs(&log), 1, "a hoist reuses the cache");
        assert_eq!(sha(&s.live().join("Barrel.glb")), cached);

        // And converting afresh in that layout makes the same textured GLB.
        s.edit(&pack_id, |steps| {
            let convert = steps.iter_mut().find(|s| s.kind == "unity_glb").unwrap();
            convert.options = json!({ "force": true });
        })
        .await;
        s.rebuilt(&pack_id).await;
        assert_eq!(runs(&log), 2);
        let fresh = base_colors(&s.live().join("Barrel.glb"));
        assert!(fresh.len() == 1 && near(fresh[0], blue), "{fresh:?}");
        assert_eq!(
            base_colors(&s.live().join("Stump.glb")),
            Vec::<[u8; 3]>::new()
        );
        s.no_work_left();
    }

    /// `Barrel.fbx`, a plane whose material `Wood` names `planks.png` by a relative path, not
    /// embedded, made by `blender` from the RGBA PNG `planks`.
    async fn barrel_fbx(blender: &Path, planks: &[u8]) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("planks.png"), planks).unwrap();
        let fbx = dir.path().join("Barrel.fbx");
        let script = format!(
            r#"
import bpy
bpy.ops.object.select_all(action="SELECT")
bpy.ops.object.delete()
bpy.ops.mesh.primitive_plane_add()
obj = bpy.context.active_object
obj.name = "Barrel"
mat = bpy.data.materials.new("Wood")
mat.use_nodes = True
tex = mat.node_tree.nodes.new("ShaderNodeTexImage")
tex.image = bpy.data.images.load({png:?})
bsdf = next(n for n in mat.node_tree.nodes if n.type == "BSDF_PRINCIPLED")
mat.node_tree.links.new(tex.outputs["Color"], bsdf.inputs["Base Color"])
obj.data.materials.append(mat)
bpy.ops.export_scene.fbx(filepath={fbx:?}, path_mode="RELATIVE", embed_textures=False)
"#,
            png = dir.path().join("planks.png").to_str().unwrap(),
            fbx = fbx.to_str().unwrap(),
        );
        let made = tokio::process::Command::new(blender)
            .args([
                "-b",
                "--factory-startup",
                "--python-exit-code",
                "1",
                "--python-expr",
            ])
            .arg(script)
            .output()
            .await
            .unwrap();
        assert!(
            made.status.success(),
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );
        // The folder it was made in, with its texture, goes away before the import.
        std::fs::read(fbx).unwrap()
    }

    /// A material's alpha mode, and its base color texture's format and first pixel.
    type Look = (Option<String>, Option<String>, Option<[u8; 3]>);

    /// What the GLB's materials look like.
    fn looks(path: &Path) -> Vec<Look> {
        let (json, _) = glb(path);
        let colors = base_colors(path);
        let mut colors = colors.into_iter();
        json["materials"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|m| {
                let texture = m["pbrMetallicRoughness"]["baseColorTexture"]["index"].as_u64();
                let mime = texture.map(|t| {
                    let source = json["textures"][t as usize]["source"].as_u64().unwrap();
                    json["images"][source as usize]["mimeType"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                });
                (
                    m["alphaMode"].as_str().map(str::to_string),
                    mime,
                    texture.and_then(|_| colors.next()),
                )
            })
            .collect()
    }

    /// An FBX's texture is found by its name in the pack, never by its path from the FBX, so a
    /// hoist that puts the texture beside the FBX reuses the cached GLB, and that GLB is what a
    /// fresh conversion in the new layout makes, alpha included.
    #[tokio::test]
    async fn an_fbx_s_texture_is_found_by_name_whatever_the_layout() {
        let s = setup().await;
        let Some(log) = s.logged_blender().await else {
            return;
        };
        let red = [200, 40, 40];
        let rgba = {
            let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([200, 40, 40, 128]));
            let mut bytes = std::io::Cursor::new(Vec::new());
            img.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
            bytes.into_inner()
        };
        let fbx = barrel_fbx(&test_blender().unwrap(), &rgba).await;
        let originals = tempfile::tempdir().unwrap();
        // No .mat, prefab or name rule matches "Wood" or "Barrel": the importer's material stays.
        let zip = zip_of(
            originals.path(),
            "barrel.zip",
            &[("Barrel.fbx", &fbx), ("Textures/planks.png", &rgba)],
        );
        let pack_id = s.import_zip_converted(&zip).await;
        assert_eq!(runs(&log), 1);
        let cached = looks(&s.live().join("Barrel.glb"));
        assert!(
            cached.len() == 1 && cached[0].2.is_some_and(|c| near(c, red)),
            "{cached:?}"
        );
        let bytes = sha(&s.live().join("Barrel.glb"));

        // Hoisting Textures puts planks.png beside Barrel.fbx: still the same GLB.
        s.edit(&pack_id, |steps| insert(steps, hoist("Textures")))
            .await;
        s.rebuilt(&pack_id).await;
        assert!(s.live().join("planks.png").is_file());
        assert_eq!(runs(&log), 1, "a hoist reuses the cache");
        assert_eq!(sha(&s.live().join("Barrel.glb")), bytes);

        // And converting afresh in that layout makes a GLB that looks the same.
        s.edit(&pack_id, |steps| {
            let convert = steps.iter_mut().find(|s| s.kind == "unity_glb").unwrap();
            convert.options = json!({ "force": true });
        })
        .await;
        s.rebuilt(&pack_id).await;
        assert_eq!(runs(&log), 2);
        assert_eq!(looks(&s.live().join("Barrel.glb")), cached);
        s.no_work_left();
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

    /// A first import of the synthetic package with `flatten`, with Settings' Blender a stub that
    /// answers `--version` as 5.2.2 and otherwise runs `body`, under a user manager that accepts
    /// every scope and runs its command, as on a desktop. Returns the run's error, once it
    /// checked the build changed nothing: no pack folder, folder beside it or work folder.
    async fn failed_with_stub_blender(s: &Setup, body: &str) -> String {
        s.stub_blender(body).await;
        let run = s
            .engine
            .start_import(s.request(vec![PlaceStep::Flatten]))
            .await
            .unwrap();
        let failed = s.wait_for(&run.id, "failed").await;
        assert!(!s.live().exists());
        assert_eq!(s.count("SELECT COUNT(*) FROM import_outputs").await, 0);
        s.no_leftovers();
        s.no_work_left();
        failed.error.unwrap()
    }

    /// Each worker request a test served, with the bytes its `source` held when it came.
    type Served = Arc<Mutex<Vec<(Value, Vec<u8>)>>>;

    /// How a stub worker answers a request.
    type Answer = fn(&Value) -> Value;

    impl Setup {
        /// Points Settings at a stub Blender that answers `--version` as 5.2.2 and otherwise runs
        /// `body`, under a user manager that accepts every scope and runs its command, as on a
        /// desktop.
        async fn stub_blender(&self, body: &str) {
            let bin = self.cache.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let blender = bin.join("blender");
            executable(
                &blender,
                &format!("[ \"$1\" = --version ] && {{ echo 'Blender 5.2.2'; exit 0; }}\n{body}"),
            );
            self.set_blender(&blender).await;
            executable(&bin.join("systemctl"), "exit 0");
            executable(
                &bin.join("systemd-run"),
                "while [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"",
            );
            self.engine.set_systemd(crate::capped::Systemd {
                run: bin.join("systemd-run"),
                ctl: bin.join("systemctl"),
            });
        }

        /// Answers the engine's preview worker requests with `answer`, given each request,
        /// and returns each request with the bytes its `source` held when it came.
        fn serve_worker(&self, answer: impl Fn(&Value) -> Value + Send + 'static) -> Served {
            let mut requests = self.worker.lock().unwrap().take().unwrap();
            let served: Served = Default::default();
            let log = served.clone();
            tokio::spawn(async move {
                while let Some(WorkerRequest { request, reply }) = requests.recv().await {
                    let source = request["params"]["source"].as_str().unwrap_or_default();
                    let bytes = std::fs::read(source).unwrap_or_default();
                    let answer = answer(&request);
                    log.lock().unwrap().push((request, bytes));
                    let _ = reply.send(Ok(answer));
                }
            });
            served
        }
    }

    /// A stub body that writes `result` as the script's result file, its last argument, and
    /// exits 0.
    fn answering(result: Value) -> String {
        format!("for last; do :; done\nprintf '%s' '{result}' > \"$last\"")
    }

    /// A12: Blender killed with status 137 under the cap fails its step, naming the cap, and the
    /// build changes nothing.
    #[tokio::test]
    async fn blender_killed_by_the_memory_cap_fails_the_step() {
        let s = setup().await;
        crate::import::settings::update(
            &s.engine.db.master_pool,
            json!({"blenderMemoryMax": "3G"}),
        )
        .await
        .unwrap();
        assert_eq!(
            failed_with_stub_blender(&s, "exit 137").await,
            "Converting to GLB failed: Blender was killed by the memory cap (3G)"
        );
    }

    /// Blender exiting with another status fails the step with its status and last lines.
    #[tokio::test]
    async fn blender_crashing_fails_the_step_with_what_it_printed() {
        let s = setup().await;
        assert_eq!(
            failed_with_stub_blender(&s, "echo 'Error: no GPU here' >&2\nexit 1").await,
            "Converting to GLB failed: Blender crashed (exit status: 1): Error: no GPU here"
        );
    }

    /// A model the script reports failed fails the step, naming the model and why.
    #[tokio::test]
    async fn a_model_the_script_fails_on_fails_the_step_naming_it() {
        let s = setup().await;
        let result = json!({
            "version": crate::import::unity_glb::UNITY_GLB_VERSION,
            "blenderVersion": "5.2.2",
            "models": [{"model": "Art/Models/SM_Log.fbx", "ok": false, "error": "no mesh in it"}],
        });
        assert_eq!(
            failed_with_stub_blender(&s, &answering(result)).await,
            "Converting Art/Models/SM_Log.fbx to GLB failed: no mesh in it"
        );
    }

    /// A result from another version of the script, one that leaves a model out, and a model
    /// reported converted with no GLB written each fail the step.
    #[tokio::test]
    async fn a_result_that_doesn_t_hold_up_fails_the_step() {
        let version = crate::import::unity_glb::UNITY_GLB_VERSION;
        let converted = json!([{"model": "Art/Models/SM_Log.fbx", "ok": true, "note": "1/1"}]);
        let cases = [
            (
                json!({"version": "opendam-unity-glb/0", "models": converted}),
                format!(
                    "Converting to GLB failed: the script reported version opendam-unity-glb/0, \
                     not {version}"
                ),
            ),
            (
                json!({"version": version, "models": []}),
                "Converting to GLB failed: Blender reported nothing for Art/Models/SM_Log.fbx"
                    .to_string(),
            ),
            (
                json!({"version": version, "models": converted}),
                "Converting Art/Models/SM_Log.fbx to GLB failed: Blender wrote no GLB".to_string(),
            ),
        ];
        for (result, error) in cases {
            let s = setup().await;
            assert_eq!(
                failed_with_stub_blender(&s, &answering(result)).await,
                error
            );
        }
    }

    /// A clean exit with no result file, and a result file that isn't the script's JSON, each
    /// fail the step with what went wrong, not as a crash.
    #[tokio::test]
    async fn a_missing_or_unreadable_result_fails_the_step() {
        let s = setup().await;
        assert_eq!(
            failed_with_stub_blender(&s, "exit 0").await,
            "Converting to GLB failed: Blender exited without a result"
        );
        let s = setup().await;
        let error =
            failed_with_stub_blender(&s, "for last; do :; done\nprintf 'not json' > \"$last\"")
                .await;
        assert!(
            error.starts_with(
                "Converting to GLB failed: Blender's conversion result can't be read: "
            ),
            "{error}"
        );
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
        // The import dialog's scan proposes it the same way.
        let original = fixture("synthetic.unitypackage");
        let scan = s
            .engine
            .scan("lib", &original.to_string_lossy())
            .await
            .unwrap();
        let proposed: Vec<bool> = scan
            .offers
            .iter()
            .flat_map(|o| &o.candidates)
            .flat_map(|c| &c.steps)
            .filter(|s| s.kind == "unity_glb")
            .map(|s| s.enabled)
            .collect();
        assert_eq!(proposed, [false]);
        assert_eq!(
            scan.unavailable.get("unity_glb").map(String::as_str),
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

    /// `body` behind the header of Unity 4.3's binary serialization, which holds the file's size.
    fn serialized(body: &[u8]) -> Vec<u8> {
        let len = 16 + body.len() as u32;
        let mut file = [0x10u32, len, 9, 16].map(u32::to_be_bytes).concat();
        file.extend(body);
        file
    }

    /// A binary Unity 4 material and prefab, as far as the step reads them: the header.
    static BINARY_MAT: LazyLock<Vec<u8>> = LazyLock::new(|| serialized(b"4.3.4f1 Material"));
    static BINARY_PREFAB: LazyLock<Vec<u8>> = LazyLock::new(|| serialized(b"4.3.4f1 GameObject"));
    /// Other programs' `.mat` files: a 3ds Max material library, and an empty one.
    const MAX_MAT: &[u8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1 3ds Max material library";
    const TEXT_MAT: &[u8] = b"%YAML 1.1\n--- !u!21 &2100000\nMaterial:\n  m_Name: M_Text\n";
    /// What the stub worker writes for a binary file.
    const AS_TEXT: &str = "%YAML 1.1\n--- !u!21 &2100000\nMaterial:\n  m_Name: from the worker\n";

    fn binary_pack(dir: &Path) -> PathBuf {
        zip_of(
            dir,
            "binary.zip",
            &[
                ("Models/SM_Log.fbx", b"an FBX the stub Blender never reads"),
                ("Materials/M_Log.mat", &BINARY_MAT[..]),
                ("Materials/M_Text.mat", TEXT_MAT),
                ("Prefabs/SM_Log.prefab", &BINARY_PREFAB[..]),
                ("Materials/Max.mat", MAX_MAT),
                ("Materials/Empty.mat", b""),
            ],
        )
    }

    /// A stub Blender body that copies the work copy it was given to `seen` and converts
    /// `Models/SM_Log.fbx`.
    fn copying_blender(seen: &Path) -> String {
        let result = json!({
            "version": crate::import::unity_glb::UNITY_GLB_VERSION,
            "models": [{"model": "Models/SM_Log.fbx", "ok": true}],
        });
        format!(
            "for last; do :; done\nwork=$(dirname \"$last\")\ncp -R \"$work/pack\" '{}'\n\
             printf glTF > \"$work/out/0.glb\"\nprintf '%s' '{result}' > \"$last\"",
            seen.display()
        )
    }

    /// Binary `.mat` and `.prefab` files reach Blender as the worker's text, the text `.mat` as
    /// it is, and the library keeps every file's own bytes.
    #[tokio::test]
    async fn binary_unity_files_reach_blender_as_text_and_the_library_keeps_their_bytes() {
        let s = setup().await;
        let seen = s.cache.path().join("seen");
        s.stub_blender(&copying_blender(&seen)).await;
        let served = s.serve_worker(|request| {
            let output = Path::new(request["params"]["output"].as_str().unwrap());
            std::fs::create_dir_all(output.parent().unwrap()).unwrap();
            std::fs::write(output, AS_TEXT).unwrap();
            json!({"id": request["id"], "result": {"ok": true, "output": output}})
        });
        let originals = tempfile::tempdir().unwrap();
        s.import_zip_converted(&binary_pack(originals.path())).await;

        let served = served.lock().unwrap().clone();
        let work = s.cache.path().join("work");
        let asked: Vec<(&str, String, &[u8])> = served
            .iter()
            .map(|(request, bytes)| {
                let source = Path::new(request["params"]["source"].as_str().unwrap());
                let in_pack = source
                    .strip_prefix(&work)
                    .expect("a work copy")
                    .components()
                    .skip(2) // the work folder and its `pack`
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                (request["method"].as_str().unwrap(), in_pack, &bytes[..])
            })
            .collect();
        assert_eq!(
            asked,
            [
                (
                    "unity_binary_to_text",
                    "Materials/M_Log.mat".to_string(),
                    &BINARY_MAT[..]
                ),
                (
                    "unity_binary_to_text",
                    "Prefabs/SM_Log.prefab".to_string(),
                    &BINARY_PREFAB[..]
                ),
            ]
        );
        for path in ["Materials/M_Log.mat", "Prefabs/SM_Log.prefab"] {
            assert_eq!(
                std::fs::read_to_string(seen.join(path)).unwrap(),
                AS_TEXT,
                "{path}"
            );
        }
        assert_eq!(
            std::fs::read(seen.join("Materials/M_Text.mat")).unwrap(),
            TEXT_MAT
        );
        // Other programs' `.mat` files aren't sent, and reach Blender as they are
        assert_eq!(
            std::fs::read(seen.join("Materials/Max.mat")).unwrap(),
            MAX_MAT
        );
        assert_eq!(
            std::fs::read(seen.join("Materials/Empty.mat")).unwrap(),
            b""
        );

        assert_eq!(
            std::fs::read(s.live().join("Materials/M_Log.mat")).unwrap(),
            &BINARY_MAT[..]
        );
        assert_eq!(
            std::fs::read(s.live().join("Prefabs/SM_Log.prefab")).unwrap(),
            &BINARY_PREFAB[..]
        );
        assert_eq!(
            std::fs::read(s.live().join("Materials/M_Text.mat")).unwrap(),
            TEXT_MAT
        );
        assert_eq!(
            std::fs::read(s.live().join("Models/SM_Log.glb")).unwrap(),
            b"glTF"
        );
        let recorded: String = sqlx::query_scalar(
            "SELECT o.sha256 FROM import_outputs o JOIN import_packs p ON p.id = o.pack_id
             AND o.build_id = p.current_build WHERE o.path = ?",
        )
        .bind(format!("{TARGET}/Materials/M_Log.mat"))
        .fetch_one(&s.lib.pool)
        .await
        .unwrap();
        assert_eq!(
            recorded,
            crate::export::hashes::sha256_bytes(&BINARY_MAT[..])
        );

        // The GLB is cached under a key holding the converter's version
        use crate::import::unity_glb::{self, Input};
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT o.path, o.source_path, o.sha256 FROM import_outputs o JOIN import_packs p
             ON p.id = o.pack_id AND o.build_id = p.current_build",
        )
        .fetch_all(&s.lib.pool)
        .await
        .unwrap();
        let inputs: Vec<Input> = rows
            .into_iter()
            .map(|(path, source, sha256)| Input {
                path: path[TARGET.len() + 1..].to_string(),
                source_path: serde_json::from_str(&source).unwrap(),
                sha256,
                guid: None,
            })
            .filter(|i| unity_glb::is_input(&i.path))
            .collect();
        let model = unity_glb::Model {
            path: "Models/SM_Log.fbx".into(),
            source_path: vec!["Models/SM_Log.fbx".into()],
            glb: "Models/SM_Log.glb".into(),
        };
        let original = sha(&originals.path().join("binary.zip"));
        let options = unity_glb::Options::parse(&json!({})).unwrap();
        let key = |converter: Option<&str>| {
            let inputs = unity_glb::inputs_hash(&inputs);
            let key = unity_glb::glb_key(&original, &model, &options, "5.2.2", &inputs, converter);
            s.cache.path().join("glb").join(format!("{key}.glb"))
        };
        assert!(key(Some(&unity_glb::converter_version())).is_file());
        assert!(!key(None).exists());
        s.no_work_left();
    }

    /// A binary file the worker can't rewrite fails the step, naming the file and why: UnityPy
    /// missing, an engine older than the request, or no engine at all. The pack stays as it was.
    #[tokio::test]
    async fn a_binary_unity_file_the_worker_can_t_rewrite_fails_the_step_naming_it() {
        let missing = |request: &Value| {
            json!({"id": request["id"], "result": {
                "ok": false, "error": "UnityPy can't be imported: No module named 'UnityPy'",
            }})
        };
        let older = |request: &Value| json!({"id": request["id"], "error": "Unknown method: unity_binary_to_text"});
        let cases: [(Option<Answer>, &str); 3] = [
            (
                Some(missing),
                "UnityPy can't be imported: No module named 'UnityPy'",
            ),
            (Some(older), "Unknown method: unity_binary_to_text"),
            (None, "the preview engine isn't running"),
        ];
        for (answer, why) in cases {
            let s = setup().await;
            s.stub_blender(&copying_blender(&s.cache.path().join("seen")))
                .await;
            match answer {
                Some(answer) => {
                    s.serve_worker(answer);
                }
                None => drop(s.worker.lock().unwrap().take()),
            }
            let originals = tempfile::tempdir().unwrap();
            let (_, run) = s
                .import_zip_then_convert(&binary_pack(originals.path()), "failed")
                .await;
            assert_eq!(
                run.error.as_deref(),
                Some(format!("Reading Materials/M_Log.mat as text failed: {why}").as_str())
            );
            assert!(!s.cache.path().join("seen").exists(), "Blender never ran");
            assert!(!s.live().join("Models/SM_Log.glb").exists());
            assert_eq!(
                std::fs::read(s.live().join("Materials/M_Log.mat")).unwrap(),
                &BINARY_MAT[..]
            );
            s.no_leftovers();
            s.no_work_left();
        }
    }
}
