//! How the Unity GLB converter finds a material's textures and which meshes it keeps (plan
//! import-material-fixes, Fixture tests): each case runs the step's `Conversion` over a pack of
//! `tests/fixtures/unity-glb/` and reads the GLB's JSON. They run Blender, from `OPENDAM_BLENDER`,
//! and skip without it outside CI.
//!
//! The tests sit in a `mod tests` block, as in every other file, so source scans that stop there
//! (`structure::tests::metadata_writers`) read none of them.

mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use serde_json::Value;
    use tokio::sync::mpsc;

    use super::super::unity_glb_tests::tests::{glb, near};
    use crate::export::blender::test_blender;
    use crate::export::hashes::sha256_file;
    use crate::import::build::RunControl;
    use crate::import::unity_glb::{Conversion, Options, Staged, Tools, DEFAULT_MEMORY_MAX};
    use crate::import::unitypackage::{SkipReason, SkippedEntry};

    // The fixtures' colours, as `make_fixtures.py` names them.
    const BASE: [u8; 3] = [200, 40, 40];
    const OTHER_BASE: [u8; 3] = [40, 200, 40];
    const NORMAL: [u8; 3] = [128, 128, 255];
    const TINT: [f64; 4] = [0.2, 0.4, 0.6, 1.0];

    /// What a mask with no base exports as: white, with the mask in its alpha, so the flat colour
    /// shows through the factor. A mask bound as a base would be the mask's black.
    const CUTOUT: ([u8; 3], u8) = ([255, 255, 255], 0);

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/unity-glb")
    }

    fn is_graph(name: &str) -> bool {
        name.ends_with(".shadergraph")
    }

    /// A conversion's GLB.
    struct Converted {
        json: Value,
        bin: Vec<u8>,
    }

    /// A material of a GLB, as the tests read it.
    #[derive(Debug)]
    struct Material {
        alpha_mode: Option<String>,
        factor: Vec<f64>,
        base: Option<([u8; 3], u8)>,
        normal: Option<([u8; 3], u8)>,
        orm: bool,
    }

    impl Converted {
        /// The first pixel of the `index`th texture, as colour and alpha.
        fn pixel(&self, index: u64) -> ([u8; 3], u8) {
            let source = self.json["textures"][index as usize]["source"]
                .as_u64()
                .unwrap() as usize;
            let view = self.json["images"][source]["bufferView"].as_u64().unwrap() as usize;
            let view = &self.json["bufferViews"][view];
            let start = view["byteOffset"].as_u64().unwrap_or(0) as usize;
            let len = view["byteLength"].as_u64().unwrap() as usize;
            let decoded = image::load_from_memory(&self.bin[start..start + len]).unwrap();
            let [r, g, b, a] = decoded.to_rgba8().get_pixel(0, 0).0;
            ([r, g, b], a)
        }

        /// The material called `name`, give or take the numeric suffix Blender adds, since the
        /// importer's material of that name is still there when the script makes its own.
        fn material(&self, name: &str) -> Material {
            let materials = self.json["materials"].as_array().cloned();
            let found = materials.unwrap_or_default().into_iter().find(|m| {
                let got = m["name"].as_str().unwrap_or_default();
                got.split('.').next() == Some(name)
            });
            let material = found.unwrap_or_else(|| {
                panic!("no {name} in {}", self.json["materials"]);
            });
            let pbr = &material["pbrMetallicRoughness"];
            Material {
                alpha_mode: material["alphaMode"].as_str().map(str::to_string),
                factor: pbr["baseColorFactor"]
                    .as_array()
                    .map(|c| c.iter().map(|v| v.as_f64().unwrap()).collect())
                    .unwrap_or_else(|| vec![1.0; 4]),
                base: pbr["baseColorTexture"]["index"]
                    .as_u64()
                    .map(|t| self.pixel(t)),
                normal: material["normalTexture"]["index"]
                    .as_u64()
                    .map(|t| self.pixel(t)),
                orm: pbr["metallicRoughnessTexture"].is_object(),
            }
        }

        /// How many images the GLB embeds.
        fn images(&self) -> usize {
            self.json["images"].as_array().map_or(0, Vec::len)
        }

        /// The GLB's nodes that hold a mesh, by name, sorted.
        fn meshes(&self) -> Vec<&str> {
            let nodes = self.json["nodes"].as_array();
            let mut names: Vec<&str> = nodes
                .into_iter()
                .flatten()
                .filter(|n| n["mesh"].is_u64())
                .map(|n| n["name"].as_str().unwrap())
                .collect();
            names.sort();
            names
        }

        /// The base color of the material on the node called `name`'s mesh.
        fn base_of(&self, name: &str) -> Option<([u8; 3], u8)> {
            let nodes = self.json["nodes"].as_array().unwrap();
            let node = nodes.iter().find(|n| n["name"] == name).unwrap();
            let mesh = &self.json["meshes"][node["mesh"].as_u64().unwrap() as usize];
            let material = mesh["primitives"][0]["material"].as_u64().unwrap() as usize;
            self.material(self.json["materials"][material]["name"].as_str().unwrap())
                .base
        }
    }

    /// What one run of the step gave.
    struct Run {
        glb: Converted,
        warnings: Vec<SkippedEntry>,
    }

    /// Runs the step over the files of the pack `case` that `keep` takes, with the conversion
    /// cache at `cache`, and returns the GLB called `name`. `None` when there's no test Blender.
    async fn run_in(case: &str, name: &str, keep: fn(&str) -> bool, cache: &Path) -> Option<Run> {
        let blender = test_blender()?;
        let staging = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for entry in std::fs::read_dir(fixtures().join(case)).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            if keep(&name) {
                let to = staging.path().join(&name);
                std::fs::copy(entry.path(), &to).unwrap();
                files.push(Staged {
                    sha256: sha256_file(&to).unwrap(),
                    source_path: vec![name.clone()],
                    path: name,
                });
            }
        }
        // No fixture holds a binary Unity file, so nothing asks the worker.
        let (worker, _requests) = mpsc::channel(1);
        let tools = Tools {
            blender_path: Some(blender.to_string_lossy().into_owned()),
            memory_max: DEFAULT_MEMORY_MAX.to_string(),
            cache_dir: cache.to_path_buf(),
            assimp: "assimp".to_string(),
            systemd: Default::default(),
            worker,
        };
        let options = Options::parse(&Options::default_json()).unwrap();
        let conversion = Conversion {
            staging: staging.path(),
            files: &files,
            original_sha256: "original",
            options: &options,
            tools: &tools,
            control: Arc::new(RunControl::default()),
            progress: Arc::new(|_| {}),
        };
        let mut warnings = Vec::new();
        conversion.run(&mut warnings).await.unwrap();
        let (json, bin) = glb(&staging.path().join(name));
        Some(Run {
            glb: Converted { json, bin },
            warnings,
        })
    }

    async fn convert_in(case: &str, keep: fn(&str) -> bool, cache: &Path) -> Option<Converted> {
        let run = run_in(case, "Model.glb", keep, cache).await?;
        Some(run.glb)
    }

    /// `run_in` over the whole pack, with a cache of its own.
    async fn run_named(case: &str, name: &str) -> Option<Run> {
        run_in(case, name, |_| true, tempfile::tempdir().unwrap().path()).await
    }

    async fn convert_without(case: &str, keep: fn(&str) -> bool) -> Option<Converted> {
        convert_in(case, keep, tempfile::tempdir().unwrap().path()).await
    }

    async fn convert(case: &str) -> Option<Converted> {
        convert_without(case, |_| true).await
    }

    fn is(pixel: Option<([u8; 3], u8)>, want: [u8; 3]) -> bool {
        pixel.is_some_and(|(color, _)| near(color, want))
    }

    /// A cutout over the flat colour, with no texture of its own showing.
    fn is_cutout(m: &Material) -> bool {
        m.alpha_mode.as_deref() == Some("MASK") && m.base == Some(CUTOUT) && is_tint(&m.factor)
    }

    fn is_tint(factor: &[f64]) -> bool {
        factor.len() == 4
            && factor
                .iter()
                .zip(TINT)
                .all(|(got, want)| (got - want).abs() < 0.01)
    }

    /// Generated names: `Texture2D_a` shown as `A_BaseColor`, with a normal and an ORM map, bind
    /// all three maps through their display names.
    #[tokio::test]
    async fn generated_names_bind_every_map_through_their_display_names() {
        let Some(glb) = convert("generated-names").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is(m.base, BASE), "{m:?}");
        assert!(is(m.normal, NORMAL), "{m:?}");
        assert!(m.orm, "{m:?}");
        assert_eq!(glb.images(), 3);
    }

    /// Graph missing, and a graph is an input: the same pack with its `.shadergraph` left out, in
    /// the same cache, converts again, to what today's rules made of a generated name: nothing
    /// but the `_Color` it holds.
    #[tokio::test]
    async fn a_graph_left_out_converts_again_with_no_display_names() {
        let cache = tempfile::tempdir().unwrap();
        let Some(with) = convert_in("generated-names", |_| true, cache.path()).await else {
            return;
        };
        assert_eq!(with.images(), 3);
        let without = convert_in("generated-names", |n| !is_graph(n), cache.path())
            .await
            .unwrap();
        let m = without.material("M_Thing");
        assert!(is_tint(&m.factor), "{m:?}");
        assert_eq!(without.images(), 0, "{m:?}");
    }

    /// A reference set by hand is the name the material uses, not the generated one.
    #[tokio::test]
    async fn a_property_is_matched_by_its_overridden_reference() {
        let Some(glb) = convert("override-reference").await else {
            return;
        };
        assert!(is(glb.material("M_Thing").base, BASE));
    }

    /// Layers: `A_BaseColor` wins over `B_BaseColor`, though the material lists B first.
    #[tokio::test]
    async fn the_first_layer_by_display_name_is_the_base() {
        let Some(glb) = convert("layers").await else {
            return;
        };
        assert!(is(glb.material("M_Thing").base, BASE));
    }

    /// ORM word: `UniformClouds` and `MaskedColorMask` hold `orm` but not the word.
    #[tokio::test]
    async fn orm_needs_the_whole_word() {
        let Some(glb) = convert("orm-word").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is(m.base, BASE) && !m.orm, "{m:?}");
    }

    /// An acronym splits from the word after it: `ORMMap` is ORM, Map.
    #[tokio::test]
    async fn an_acronym_is_a_word_of_its_own() {
        let Some(glb) = convert("orm-map").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is(m.base, BASE) && m.orm, "{m:?}");
    }

    /// Detail normal: `Detail Map Normal` sorts first but isn't the normal map.
    #[tokio::test]
    async fn a_detail_normal_is_not_the_normal_map() {
        let Some(glb) = convert("detail-normal").await else {
            return;
        };
        assert!(is(glb.material("M_Thing").normal, NORMAL));
    }

    /// `Second Normal` sorts first but isn't the normal map, as `Detail Map Normal` isn't.
    #[tokio::test]
    async fn a_second_normal_is_not_the_normal_map() {
        let Some(glb) = convert("second-normal").await else {
            return;
        };
        assert!(is(glb.material("M_Thing").normal, NORMAL));
    }

    /// The plural is a normal map too.
    #[tokio::test]
    async fn normals_is_a_normal_word() {
        let Some(glb) = convert("normals-word").await else {
            return;
        };
        assert!(is(glb.material("M_Thing").normal, NORMAL));
    }

    /// `Opacity` is an opacity word, as `Alpha` is.
    #[tokio::test]
    async fn opacity_is_an_opacity_word() {
        let Some(glb) = convert("opacity-word").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert_eq!(m.alpha_mode.as_deref(), Some("MASK"), "{m:?}");
        assert!(is(m.base, BASE) && m.base.unwrap().1 == 0, "{m:?}");
    }

    /// Mask only: an `OpacityMap` and a colour, no base: a cutout, with the colour as its factor.
    #[tokio::test]
    async fn a_mask_with_no_base_cuts_out_over_the_flat_colour() {
        let Some(glb) = convert("mask-only").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is_cutout(&m), "{m:?}");
    }

    /// Helper mask: `GlitterMask` beside a base isn't opacity.
    #[tokio::test]
    async fn a_helper_mask_leaves_the_material_opaque() {
        let Some(glb) = convert("helper-mask").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is(m.base, BASE) && m.alpha_mode.is_none(), "{m:?}");
    }

    /// Base and mask, the foliage cutout: the base's colour, cut by the separate mask.
    #[tokio::test]
    async fn a_mask_beside_a_base_cuts_it_out() {
        let Some(glb) = convert("base-and-mask").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert_eq!(m.alpha_mode.as_deref(), Some("MASK"), "{m:?}");
        assert!(is(m.base, BASE) && m.base.unwrap().1 == 0, "{m:?}");
    }

    /// Raw opacity names cut out with a base and with none, and come before a display name.
    #[tokio::test]
    async fn a_raw_opacity_name_is_a_mask() {
        let Some(glb) = convert("raw-mask").await else {
            return;
        };
        for name in ["M_Base", "M_First"] {
            let m = glb.material(name);
            assert_eq!(m.alpha_mode.as_deref(), Some("MASK"), "{name}: {m:?}");
            assert!(is(m.base, BASE) && m.base.unwrap().1 == 0, "{name}: {m:?}");
        }
        let m = glb.material("M_Flat");
        assert!(is_cutout(&m), "{m:?}");
    }

    /// A cutout with no mask reads its base's alpha, and with no base it has nothing to cut with.
    #[tokio::test]
    async fn a_cutout_with_no_mask_uses_its_base_s_alpha() {
        let Some(glb) = convert("cutout").await else {
            return;
        };
        let m = glb.material("M_Base");
        assert_eq!(m.alpha_mode.as_deref(), Some("MASK"), "{m:?}");
        assert!(is(m.base, BASE) && m.base.unwrap().1 == 0, "{m:?}");
        let m = glb.material("M_Flat");
        assert!(m.alpha_mode.is_none() && m.base.is_none(), "{m:?}");
        assert!(is_tint(&m.factor), "{m:?}");
    }

    /// A base with `Alpha` in its display name isn't its own mask: an opaque material stays
    /// opaque, and a cutout reads the texture's alpha, not its high red channel.
    #[tokio::test]
    async fn a_base_with_alpha_in_its_display_name_is_not_its_own_mask() {
        for case in ["base-alpha-raw", "base-alpha-generated"] {
            let Some(glb) = convert(case).await else {
                return;
            };
            let m = glb.material("M_Opaque");
            assert!(is(m.base, BASE) && m.alpha_mode.is_none(), "{case}: {m:?}");
            let m = glb.material("M_Cutout");
            assert_eq!(m.alpha_mode.as_deref(), Some("MASK"), "{case}: {m:?}");
            assert!(is(m.base, BASE), "{case}: {m:?}");
            assert_eq!(m.base.unwrap().1, 0, "{case}: {m:?}");
        }
    }

    /// `Albedo Alpha` sorts before `OpacityMap`, whose texture is still the mask.
    #[tokio::test]
    async fn a_real_mask_is_found_past_a_base_with_alpha_in_its_name() {
        let Some(glb) = convert("base-alpha-and-mask").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert_eq!(m.alpha_mode.as_deref(), Some("MASK"), "{m:?}");
        assert!(is(m.base, BASE) && m.base.unwrap().1 == 0, "{m:?}");
    }

    /// Blank twin, Twin with no .mat, Twin over colours, and a prefab naming the blank `MI_X`
    /// for a slot of another name: each uses `M_X`'s texture.
    #[tokio::test]
    async fn a_blank_mi_slot_uses_its_textured_m_twin() {
        for case in [
            "blank-twin",
            "twin-no-mat",
            "twin-lower-case",
            "twin-over-colours",
            "twin-from-prefab",
        ] {
            let Some(glb) = convert(case).await else {
                return;
            };
            assert!(is(glb.material("M_Wall").base, BASE), "{case}");
        }
    }

    /// Only an `MI_X` has a twin: `SM_Wall` keeps its colour beside a textured `M_Wall`.
    #[tokio::test]
    async fn a_slot_not_named_mi_has_no_twin() {
        let Some(glb) = convert("not-a-twin").await else {
            return;
        };
        let m = glb.material("SM_Wall");
        assert!(is_tint(&m.factor) && m.base.is_none(), "{m:?}");
        assert_eq!(glb.images(), 0, "{m:?}");
    }

    /// An `MI_X` with its own texture keeps it, though `M_X` has one too.
    #[tokio::test]
    async fn a_textured_mi_slot_keeps_its_own_texture() {
        let Some(glb) = convert("textured-mi").await else {
            return;
        };
        assert!(is(glb.material("MI_Wall").base, OTHER_BASE));
    }

    /// The twin comes before a texture named after the slot.
    #[tokio::test]
    async fn the_twin_comes_before_a_texture_named_after_the_slot() {
        let Some(glb) = convert("twin-before-named").await else {
            return;
        };
        assert!(is(glb.material("M_Wall").base, BASE));
    }

    /// With no twin, a texture named after the slot is the last resort, for a slot with no `.mat`
    /// and for a blank one, and a `.mat` with colours keeps them.
    #[tokio::test]
    async fn a_texture_named_after_the_slot_is_the_last_resort() {
        let Some(glb) = convert("named-last").await else {
            return;
        };
        for name in ["MI_Gone", "MI_Blank"] {
            assert!(is(glb.material(name).base, BASE), "{name}");
        }
        let m = glb.material("MI_Stone");
        assert!(is_tint(&m.factor) && m.base.is_none(), "{m:?}");
    }

    /// True flat colour: `MI_Stone` has no `M_Stone`, and `MI_Roof`'s `M_Roof` has no texture, so
    /// each keeps its own colour.
    #[tokio::test]
    async fn an_mi_slot_with_no_textured_twin_keeps_its_colour() {
        let Some(glb) = convert("true-flat-colour").await else {
            return;
        };
        for name in ["MI_Stone", "MI_Roof"] {
            let m = glb.material(name);
            assert!(is_tint(&m.factor) && m.base.is_none(), "{name}: {m:?}");
        }
        assert_eq!(glb.images(), 0);
    }

    /// Flat colour: `BaseColor Value` beats the `_BaseColor` and `_Color` beside it.
    #[tokio::test]
    async fn a_display_name_gives_the_flat_colour() {
        let Some(glb) = convert("flat-colour").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is_tint(&m.factor) && m.base.is_none(), "{m:?}");
    }

    /// A raw colour name beats a display name of the same word.
    #[tokio::test]
    async fn a_raw_colour_comes_before_a_display_name() {
        let Some(glb) = convert("raw-colour-first").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is_tint(&m.factor), "{m:?}");
    }

    /// Of two colours whose display names read the same, the one that sorts first wins.
    #[tokio::test]
    async fn colours_read_their_display_names_in_order() {
        let Some(glb) = convert("colour-sort").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is_tint(&m.factor), "{m:?}");
    }

    /// Only texture: `_Grid` alone is the base.
    #[tokio::test]
    async fn a_material_s_only_texture_is_its_base() {
        let Some(glb) = convert("only-texture").await else {
            return;
        };
        assert!(is(glb.material("M_Thing").base, BASE));
    }

    /// A base hint beside another role's word isn't a base: not `Albedo Mask`, which sorts first,
    /// and not a raw `_ColorMask`.
    #[tokio::test]
    async fn a_name_with_a_base_hint_and_another_role_s_word_is_not_the_base() {
        for case in ["display-not-base", "raw-not-base"] {
            let Some(glb) = convert(case).await else {
                return;
            };
            assert!(is(glb.material("M_Thing").base, BASE), "{case}");
        }
    }

    /// A raw name's base hint comes before a display name's.
    #[tokio::test]
    async fn a_raw_base_hint_comes_before_a_display_name_s() {
        let Some(glb) = convert("raw-hint-first").await else {
            return;
        };
        assert!(is(glb.material("M_Thing").base, BASE));
    }

    /// Two textures nothing claims: neither is the only texture, so the flat colour stays.
    #[tokio::test]
    async fn two_unclaimed_textures_leave_the_flat_colour() {
        let Some(glb) = convert("two-textures").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is_tint(&m.factor) && m.base.is_none(), "{m:?}");
    }

    /// A lone `Texture2D_x` with a neutral display name is the material's only texture.
    #[tokio::test]
    async fn a_lone_generated_name_with_a_display_name_is_the_base() {
        let Some(glb) = convert("lone-neutral").await else {
            return;
        };
        assert!(is(glb.material("M_Thing").base, BASE));
    }

    /// Generated slot: a lone `_SampleTexture2D_..._Texture_1` is no base.
    #[tokio::test]
    async fn a_generated_sample_slot_is_no_base() {
        let Some(glb) = convert("generated-slot").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is_tint(&m.factor) && glb.images() == 0, "{m:?}");
    }

    /// Lone mask: a lone `Texture2D_x` shown as `OpacityMap` is a cutout over the flat colour, not
    /// a base.
    #[tokio::test]
    async fn a_lone_opacity_map_is_a_cutout_not_a_base() {
        let Some(glb) = convert("lone-mask").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is_cutout(&m), "{m:?}");
    }

    /// A lone texture with a `NOT_BASE` word and no role is no base, by raw name or display name.
    #[tokio::test]
    async fn a_lone_texture_with_a_not_base_word_is_not_a_base() {
        let Some(glb) = convert("lone-not-base").await else {
            return;
        };
        for name in ["M_Raw", "M_Shown"] {
            let m = glb.material(name);
            assert!(m.base.is_none() && is_tint(&m.factor), "{name}: {m:?}");
        }
    }

    /// Lone normal: a lone `Texture2D_x` shown as `NormalMap` is a normal map, not a base.
    #[tokio::test]
    async fn a_lone_normal_map_is_not_a_base() {
        let Some(glb) = convert("lone-normal").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is(m.normal, NORMAL) && m.base.is_none(), "{m:?}");
    }

    /// A lone texture another role takes is no base, though no `NOT_BASE` word says so: `Alpha`
    /// is a mask, and `_ARM` an ORM map.
    #[tokio::test]
    async fn a_lone_texture_with_another_role_is_not_a_base() {
        let Some(glb) = convert("lone-alpha").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is_cutout(&m), "{m:?}");
        let Some(glb) = convert("lone-arm").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(m.orm && m.base.is_none() && is_tint(&m.factor), "{m:?}");
    }

    /// Lone, graph missing: with no display name a lone `Texture2D_x` stays unresolved, so even
    /// its mask is lost, as today. The same goes for an older graph, an unreadable one, one with
    /// names or references that aren't strings, and one that names it only in a keyword, which
    /// is no property.
    #[tokio::test]
    async fn a_lone_generated_name_with_no_display_name_stays_unresolved() {
        let Some(glb) = convert_without("lone-mask", |n| !is_graph(n)).await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(m.alpha_mode.is_none() && is_tint(&m.factor), "{m:?}");
        assert_eq!(glb.images(), 0, "{m:?}");
        for case in [
            "older-graph",
            "unreadable-graph",
            "odd-graph",
            "keyword-graph",
        ] {
            let Some(glb) = convert(case).await else {
                return;
            };
            let m = glb.material("M_Thing");
            assert!(is_tint(&m.factor) && glb.images() == 0, "{case}: {m:?}");
        }
    }

    /// Which of a pack's meshes `Model.glb` keeps, with no warning.
    async fn kept(case: &str) -> Option<Vec<String>> {
        let run = run_named(case, "Model.glb").await?;
        assert_eq!(run.warnings, [], "{case}");
        Some(run.glb.meshes().iter().map(|n| n.to_string()).collect())
    }

    /// LOD only: a model holding only `_LOD1` meshes keeps them, with no warning.
    #[tokio::test]
    async fn a_model_holding_only_lod1_meshes_keeps_them() {
        let Some(meshes) = kept("lod-only").await else {
            return;
        };
        assert_eq!(meshes, ["Model_LOD1"]);
    }

    /// Mixed LODs: each mesh keeps its own lowest level, whichever that is.
    #[tokio::test]
    async fn each_mesh_keeps_its_own_lowest_level() {
        let Some(meshes) = kept("lod-mixed").await else {
            return;
        };
        assert_eq!(meshes, ["A_LOD0", "B_LOD1", "C_LOD2"]);
    }

    /// No suffix: `Wall` is level 0, so `Wall_LOD1` goes, and `Roof` and `Roof_LOD0` are one level.
    #[tokio::test]
    async fn no_suffix_and_lod0_are_both_level_0() {
        let Some(meshes) = kept("lod-no-suffix").await else {
            return;
        };
        assert_eq!(meshes, ["Roof", "Roof_LOD0", "Wall"]);
    }

    /// Several at one level: every object at the group's lowest level stays.
    #[tokio::test]
    async fn every_object_at_the_lowest_level_stays() {
        let Some(meshes) = kept("lod-several").await else {
            return;
        };
        assert_eq!(meshes, ["X_LOD1.001", "X_LOD1.002"]);
    }

    /// LOD file: a file that is itself a LOD keeps all, its name's level and any other.
    #[tokio::test]
    async fn a_file_that_is_a_lod_keeps_all() {
        let Some(run) = run_named("lod-file", "X_LOD1.glb").await else {
            return;
        };
        assert_eq!(run.glb.meshes(), ["X_LOD1", "X_LOD2"]);
        assert_eq!(run.warnings, []);
    }

    /// The GLB in `cache`, with its modified time, so a second run can't pass by converting.
    fn cached(cache: &Path) -> (PathBuf, std::time::SystemTime) {
        let mut glbs = std::fs::read_dir(cache.join("glb")).unwrap();
        let glb = glbs.next().unwrap().unwrap();
        assert!(glbs.next().is_none());
        (glb.path(), glb.metadata().unwrap().modified().unwrap())
    }

    /// No mesh, and No mesh, cache hit: a model left with only collision meshes warns, naming
    /// its source path, on the run that converts it and on the one that finds it in the cache.
    #[tokio::test]
    async fn a_model_left_with_no_mesh_warns_on_every_run() {
        let cache = tempfile::tempdir().unwrap();
        let Some(first) = run_in("no-mesh", "Model.glb", |_| true, cache.path()).await else {
            return;
        };
        let warning = SkippedEntry {
            source_path: vec!["Model.obj".to_string()],
            reason: SkipReason::NoMesh,
        };
        assert_eq!(first.glb.meshes(), Vec::<&str>::new());
        assert_eq!(first.warnings, std::slice::from_ref(&warning));
        let was = cached(cache.path());
        let second = run_in("no-mesh", "Model.glb", |_| true, cache.path())
            .await
            .unwrap();
        assert_eq!(cached(cache.path()), was, "the second run read the cache");
        assert_eq!(second.warnings, [warning]);
    }

    /// Collider file: a model named as a collider is the known gap, and stays silent on the run
    /// that converts it and on the one that finds it in the cache.
    #[tokio::test]
    async fn a_model_named_as_a_collider_stays_silent_on_every_run() {
        let cache = tempfile::tempdir().unwrap();
        let name = "rock_collider.glb";
        let Some(first) = run_in("collider-file", name, |_| true, cache.path()).await else {
            return;
        };
        assert_eq!(first.glb.meshes(), Vec::<&str>::new());
        assert_eq!(first.warnings, []);
        let was = cached(cache.path());
        let second = run_in("collider-file", name, |_| true, cache.path())
            .await
            .unwrap();
        assert_eq!(cached(cache.path()), was, "the second run read the cache");
        assert_eq!(second.warnings, []);
    }

    /// Exact name: `Plane.099` and `Plane.100` each find their own prefab's material through the
    /// exact names their `.meta` maps, which `Plane` isn't.
    #[tokio::test]
    async fn an_object_named_exactly_in_a_meta_finds_its_own_material() {
        let Some(run) = run_named("exact-name", "Model.glb").await else {
            return;
        };
        assert_eq!(run.glb.meshes(), ["Plane.099", "Plane.100"]);
        assert!(is(run.glb.base_of("Plane.099"), BASE));
        assert!(is(run.glb.base_of("Plane.100"), OTHER_BASE));
    }

    /// A byte order mark doesn't hide a graph's names.
    #[tokio::test]
    async fn a_graph_with_a_byte_order_mark_is_read() {
        let Some(glb) = convert("bom-graph").await else {
            return;
        };
        assert!(is(glb.material("M_Thing").base, BASE));
    }

    /// Raw names stay first: `_BaseMap` and `_Normal` keep their textures, though the graph shows
    /// them as `Detail` and `Second` and shows others as `A_BaseColor` and `A_Normal`.
    #[tokio::test]
    async fn raw_names_come_before_display_names() {
        let Some(glb) = convert("raw-first").await else {
            return;
        };
        let m = glb.material("M_Thing");
        assert!(is(m.base, BASE) && is(m.normal, NORMAL), "{m:?}");
    }
}
