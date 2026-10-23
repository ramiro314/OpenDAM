//! What a Unity pack's own files say about a model's materials (plan game-export C7 step 2): a
//! `.meta` file's GUID, a model's `.fbx.meta` material remap, and a `.mat` file's textures and
//! alpha clip. Unity writes these as YAML in a fixed shape, so each is read line by line rather
//! than with a YAML parser.

use std::collections::BTreeMap;

use crate::export::textures::{Layout, Role};

/// The `guid` a `.meta` file gives its asset.
pub fn meta_guid(meta: &str) -> Option<String> {
    meta.lines().find_map(|line| {
        let guid = line.strip_prefix("guid:")?.trim();
        (!guid.is_empty()).then(|| guid.to_ascii_lowercase())
    })
}

/// The value of `key` in an inline YAML map such as `{fileID: 2800000, guid: abc, type: 3}`.
fn inline_value<'a>(map: &'a str, key: &str) -> Option<&'a str> {
    let inner = map.trim().strip_prefix('{')?.strip_suffix('}')?;
    inner.split(',').find_map(|pair| {
        let (k, v) = pair.split_once(':')?;
        (k.trim() == key).then(|| v.trim())
    })
}

/// The GUID an inline object reference names, or `None` for an empty one (`{fileID: 0}`).
fn reference_guid(map: &str) -> Option<String> {
    let guid = inline_value(map, "guid")?;
    (!guid.is_empty() && guid.bytes().any(|b| b != b'0')).then(|| guid.to_ascii_lowercase())
}

/// A model `.meta`'s `externalObjects` remap: each material name it remaps, with the GUID of
/// the `.mat` it uses instead.
pub fn material_remap(meta: &str) -> BTreeMap<String, String> {
    let mut remap = BTreeMap::new();
    let mut in_block = false;
    let (mut is_material, mut name) = (false, None::<String>);
    for line in meta.lines() {
        let trimmed = line.trim();
        if !in_block {
            in_block = trimmed == "externalObjects:";
            continue;
        }
        // The block ends at the next key that isn't indented under it
        if !line.starts_with("  ") || (!line.starts_with("   ") && !trimmed.starts_with('-')) {
            break;
        }
        if trimmed.starts_with("- first:") {
            (is_material, name) = (false, None);
        } else if let Some(kind) = trimmed.strip_prefix("type:") {
            is_material = kind.trim() == "UnityEngine:Material";
        } else if let Some(n) = trimmed.strip_prefix("name:") {
            name = Some(n.trim().to_string());
        } else if let Some(second) = trimmed.strip_prefix("second:") {
            if let (true, Some(name), Some(guid)) =
                (is_material, name.take(), reference_guid(second))
            {
                remap.insert(name, guid);
            }
        }
    }
    remap
}

/// A `.mat` file's texture properties, by property name with the GUID of the texture each
/// names, and whether it clips alpha.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Material {
    pub textures: BTreeMap<String, String>,
    pub alpha_clip: bool,
}

/// The floats that mean alpha clip is on (D23): URP's `_AlphaClip`, Standard's cutout `_Mode`
/// and HDRP's `_AlphaCutoffEnable`, each 1.
const CLIP_FLOATS: [&str; 3] = ["_AlphaClip", "_Mode", "_AlphaCutoffEnable"];

/// Reads a `.mat` file.
pub fn read_material(mat: &str) -> Material {
    #[derive(PartialEq)]
    enum Section {
        Other,
        Textures,
        Numbers,
    }
    let mut material = Material::default();
    let mut section = Section::Other;
    let mut property: Option<String> = None;
    for line in mat.lines() {
        let trimmed = line.trim();
        // `m_TexEnvs`, `m_Ints`, `m_Floats` and `m_Colors` sit at 4 spaces, below
        // `m_SavedProperties`, and their items' keys deeper
        if trimmed.starts_with("m_") && line.len() - line.trim_start().len() <= 4 {
            section = match trimmed.split(':').next() {
                Some("m_TexEnvs") => Section::Textures,
                Some("m_Floats" | "m_Ints") => Section::Numbers,
                _ => Section::Other,
            };
            property = None;
            continue;
        }
        let Some(item) = trimmed.strip_prefix("- ") else {
            if section == Section::Textures {
                if let (Some(prop), Some(map)) = (&property, trimmed.strip_prefix("m_Texture:")) {
                    if let Some(guid) = reference_guid(map) {
                        material.textures.insert(prop.clone(), guid);
                    }
                }
            }
            continue;
        };
        let Some((key, value)) = item.split_once(':') else {
            continue;
        };
        match section {
            Section::Textures => property = Some(key.trim().to_string()),
            Section::Numbers => {
                let on = value.trim().parse::<f64>().is_ok_and(|v| v == 1.0);
                if on && CLIP_FLOATS.contains(&key.trim()) {
                    material.alpha_clip = true;
                }
            }
            Section::Other => {}
        }
    }
    material
}

/// The layout a URP, Standard or HDRP texture property holds (C7 step 2), or `None` for one the
/// export doesn't read.
pub fn property_layout(property: &str) -> Option<Layout> {
    Some(match property {
        "_BaseMap" | "_MainTex" | "_BaseColorMap" => Layout::Role(Role::BaseColor),
        "_BumpMap" | "_NormalMap" => Layout::Role(Role::Normal),
        "_MetallicGlossMap" => Layout::Role(Role::MetallicSmoothness),
        "_MaskMap" => Layout::MaskMap,
        "_OcclusionMap" => Layout::Role(Role::Occlusion),
        "_EmissionMap" | "_EmissiveColorMap" => Layout::Role(Role::Emission),
        "_ParallaxMap" | "_HeightMap" => Layout::Role(Role::Height),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/game-export/library/Unity Moss"
    );

    fn read(path: &str) -> String {
        std::fs::read_to_string(format!("{FIXTURE}/{path}")).unwrap()
    }

    #[test]
    fn a_meta_file_gives_its_guid() {
        assert_eq!(
            meta_guid(&read("T_Moss_D.png.meta")).as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            meta_guid("fileFormatVersion: 2\nguid: ABCDEF\n").as_deref(),
            Some("abcdef")
        );
        assert_eq!(meta_guid("fileFormatVersion: 2\n"), None);
    }

    #[test]
    fn a_model_meta_remaps_materials_by_name() {
        assert_eq!(
            material_remap(&read("Models/SM_Log.fbx.meta")),
            BTreeMap::from([("Bark".into(), "c0000000000000000000000000000001".into())])
        );
        // Only materials, and only with a GUID
        let meta = "ModelImporter:\n  externalObjects:\n  - first:\n      type: UnityEngine:Material\n      assembly: UnityEngine.CoreModule\n      name: Iron\n    second: {fileID: 0}\n  - first:\n      type: UnityEngine:Texture2D\n      name: Rust\n    second: {fileID: 2800000, guid: aa11, type: 3}\n  - first:\n      type: UnityEngine:Material\n      name: Wood\n    second: {fileID: 2100000, guid: BB22, type: 2}\n  materials:\n    materialImportMode: 2\n";
        assert_eq!(
            material_remap(meta),
            BTreeMap::from([("Wood".into(), "bb22".into())])
        );
        assert_eq!(
            material_remap("ModelImporter:\n  externalObjects: {}\n"),
            BTreeMap::new()
        );
    }

    #[test]
    fn a_mat_file_gives_its_textures_and_alpha_clip() {
        let bark = read_material(&read("Materials/M_LogBark.mat"));
        assert_eq!(
            bark.textures,
            BTreeMap::from([
                ("_BaseMap".into(), "b0000000000000000000000000000001".into()),
                ("_BumpMap".into(), "b0000000000000000000000000000002".into()),
            ])
        );
        assert!(!bark.alpha_clip);
        let leaves = read_material(&read("Materials/Leaves.mat"));
        assert_eq!(leaves.textures.len(), 1);
        assert!(leaves.alpha_clip);

        // Standard's cutout mode and HDRP's alpha cutoff clip too; other floats don't
        let floats = |line: &str| {
            read_material(&format!(
                "Material:\n  m_SavedProperties:\n    m_TexEnvs: []\n    m_Floats:\n    {line}\n    m_Colors: []\n"
            ))
            .alpha_clip
        };
        assert!(floats("- _Mode: 1"));
        assert!(floats("- _AlphaCutoffEnable: 1"));
        assert!(!floats("- _Mode: 2"));
        assert!(!floats("- _Cutoff: 1"));
    }

    #[test]
    fn texture_properties_map_to_layouts() {
        assert_eq!(
            property_layout("_MainTex"),
            Some(Layout::Role(Role::BaseColor))
        );
        assert_eq!(property_layout("_MaskMap"), Some(Layout::MaskMap));
        assert_eq!(
            property_layout("_MetallicGlossMap"),
            Some(Layout::Role(Role::MetallicSmoothness))
        );
        assert_eq!(property_layout("_DetailAlbedoMap"), None);
    }
}
