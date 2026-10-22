//! Packs and provenance in a manifest (plan game-export C6): each pack's id, name, version,
//! publisher, source URL, licence, origin and normal-map convention, read from what the
//! structure pass derived (`PackInfo`), your `pack_overrides` edits and, for the origin, the
//! pack's files at read time (A1).

use std::path::Path;

use serde::Serialize;
use serde_json::{json, Value};

use crate::export::conversion::Walker;
use crate::export::textures::Convention;
use crate::structure::{spdx_kind, LicenseKind, PackInfo, PackOverride};

/// A pack as the export reads it, before it has an id.
#[derive(Debug, Clone)]
pub struct PackSource {
    pub library_id: String,
    /// Relative to the library root, `""` for a root pack
    pub path: String,
    /// The folder's name, or the library's for a root pack
    pub folder_name: String,
    pub info: PackInfo,
    pub edits: PackOverride,
    /// The `datapackage.json` `name`, which the structure pass doesn't keep
    pub package_name: Option<String>,
    pub origin: Origin,
}

/// Where a pack's files come from (D12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    Unity,
    Unreal,
    Generic,
}

impl Origin {
    /// The pack's normal-map convention (D14): DirectX for unreal, else OpenGL.
    pub fn normal_convention(self) -> Convention {
        match self {
            Origin::Unreal => Convention::DirectX,
            _ => Convention::OpenGl,
        }
    }
}

/// A manifest pack's `licence`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Licence {
    pub spdx: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A pack as it goes into a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportPack {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    pub licence: Licence,
    pub origin: Origin,
}

impl ExportPack {
    pub fn manifest_entry(&self) -> Value {
        json!(self)
    }
}

/// Whether `id` matches the contract's pack id, `^[a-z0-9][a-z0-9-]*$`.
fn is_slug(id: &str) -> bool {
    id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A pack's id before clashes: the `datapackage.json` `name` when it is a valid slug, else the
/// folder's name lowercased with each run of other characters turned into `-` and `-` trimmed
/// from the ends, else `pack`.
pub fn natural_id(package_name: Option<&str>, folder_name: &str) -> String {
    if let Some(name) = package_name.filter(|n| is_slug(n)) {
        return name.to_string();
    }
    let mut slug = String::new();
    for c in folder_name.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "pack".into()
    } else {
        slug.into()
    }
}

/// Every pack's id, given each pack's natural id in manifest order (library order, then path):
/// the first pack with an id keeps it, and each later one takes the lowest `-2`, `-3`, ... suffix
/// no other pack holds, natural or suffixed. So `foo`, `foo` and `foo-2` give `foo`, `foo-3` and
/// `foo-2`.
pub fn assign_ids(natural: &[String]) -> Vec<String> {
    let mut taken: std::collections::HashSet<String> = natural.iter().cloned().collect();
    let mut kept = std::collections::HashSet::new();
    natural
        .iter()
        .map(|id| {
            if kept.insert(id.clone()) {
                return id.clone();
            }
            let suffixed = (2..)
                .map(|n| format!("{id}-{n}"))
                .find(|candidate| !taken.contains(candidate))
                .expect("some suffix is free");
            taken.insert(suffixed.clone());
            kept.insert(suffixed.clone());
            suffixed
        })
        .collect()
}

/// SPDX ids beyond the ones `spdx_kind` knows that asset licences use, lowercase.
const OTHER_SPDX_IDS: [&str; 21] = [
    "0bsd",
    "agpl-3.0-only",
    "agpl-3.0-or-later",
    "bsd-2-clause",
    "bsd-3-clause",
    "cc-pddc",
    "gpl-2.0-only",
    "gpl-2.0-or-later",
    "gpl-3.0-only",
    "gpl-3.0-or-later",
    "isc",
    "lgpl-2.1-only",
    "lgpl-2.1-or-later",
    "lgpl-3.0-only",
    "lgpl-3.0-or-later",
    "mit-0",
    "mpl-2.0",
    "unlicense",
    "wtfpl",
    "zlib",
    "artistic-2.0",
];

/// The Creative Commons families `spdx_kind` calls unknown, each followed by a version.
const OTHER_CC_FAMILIES: [&str; 4] = ["cc-by-nc-", "cc-by-nd-", "cc-by-nc-sa-", "cc-by-nc-nd-"];

/// Whether `id` is an SPDX license id or a `LicenseRef-` reference, ignoring case: one
/// `spdx_kind` recognises, a versioned non-commercial or no-derivatives Creative Commons id, or
/// another common id (`OTHER_SPDX_IDS`).
pub fn is_spdx_id(id: &str) -> bool {
    let id = id.trim();
    let lower = id.to_ascii_lowercase();
    if let Some(reference) = lower.strip_prefix("licenseref-") {
        return !reference.is_empty()
            && reference
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    }
    spdx_kind(id) != LicenseKind::Unknown
        || OTHER_SPDX_IDS.contains(&lower.as_str())
        || OTHER_CC_FAMILIES.iter().any(|family| {
            lower
                .strip_prefix(family)
                .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit()))
        })
}

/// The SPDX id of a kind that has a single version. CC-BY, CC-BY-SA and OFL name no version, so
/// they give none.
fn kind_spdx(kind: LicenseKind) -> Option<&'static str> {
    match kind {
        LicenseKind::Cc0 => Some("CC0-1.0"),
        LicenseKind::Mit => Some("MIT"),
        LicenseKind::Apache2 => Some("Apache-2.0"),
        _ => None,
    }
}

fn kind_name(kind: LicenseKind) -> Option<&'static str> {
    match kind {
        LicenseKind::Cc0 => Some("CC0"),
        LicenseKind::CcBy => Some("CC-BY"),
        LicenseKind::CcBySa => Some("CC-BY-SA"),
        LicenseKind::Mit => Some("MIT"),
        LicenseKind::Apache2 => Some("Apache-2.0"),
        LicenseKind::Ofl => Some("OFL"),
        LicenseKind::Unknown => None,
    }
}

/// A pack's licence (D15, A8). `spdx` is the first `datapackage.json` licence `name` that is an
/// SPDX id or a `LicenseRef-` (as written), else the id of the kind the pack's licence names, else
/// your licence text when it is an SPDX id, else null. `name` is the `datapackage.json` licence's
/// title, else your text, else its `name`, else the kind's name, else `Unknown`. `url` is the
/// `datapackage.json` licence's http(s) URL.
pub fn licence(info: &PackInfo, edits: &PackOverride) -> Licence {
    let package = info.package.as_ref();
    let shown = package.and_then(|p| p.license.as_ref());
    let kind = shown
        .map(|l| l.kind)
        .or_else(|| Some(info.license.as_ref()?.text.kind))
        .unwrap_or_default();
    let yours = edits
        .license
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    let spdx = package
        .into_iter()
        .flat_map(|p| &p.meta.licenses)
        .filter_map(|l| l.id.as_deref())
        .find(|id| is_spdx_id(id))
        .or_else(|| kind_spdx(kind))
        .or_else(|| yours.filter(|y| is_spdx_id(y)))
        .map(str::to_string);
    let name = shown
        .and_then(|l| l.title.as_deref())
        .or(yours)
        .or_else(|| shown.and_then(|l| l.id.as_deref()))
        .or_else(|| kind_name(kind))
        .unwrap_or("Unknown")
        .to_string();
    Licence {
        spdx,
        name,
        url: shown.and_then(|l| l.url.clone()),
    }
}

/// The `name` of the `datapackage.json` at `path` (relative to `root`), if the file reads and
/// has a string `name`.
pub fn read_package_name(root: &Path, path: &str) -> Option<String> {
    let bytes = std::fs::read(root.join(path)).ok()?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    let value: Value = serde_json::from_slice(bytes).ok()?;
    Some(value.get("name")?.as_str()?.trim().to_string()).filter(|n| !n.is_empty())
}

/// A pack's inferred origin (D12, A1), from its indexed `files` (relative to `root`): `unity` when
/// any has a `<file>.meta` beside it, else `unreal` when the nearest valid `conversion.json` of any
/// has a step `from: "uasset"`, else `generic`.
pub fn infer_origin(root: &Path, files: &[String], walker: &mut Walker) -> Origin {
    if files
        .iter()
        .any(|f| root.join(format!("{f}.meta")).is_file())
    {
        return Origin::Unity;
    }
    if files.iter().any(|f| {
        walker
            .nearest_for_file(f)
            .is_some_and(|n| n.has_step_from("uasset"))
    }) {
        return Origin::Unreal;
    }
    Origin::Generic
}

/// `sources`, in manifest order, as manifest packs with their ids assigned.
pub fn export_packs(sources: &[PackSource]) -> Vec<ExportPack> {
    let natural: Vec<String> = sources
        .iter()
        .map(|s| natural_id(s.package_name.as_deref(), &s.folder_name))
        .collect();
    let meta = |s: &PackSource| s.info.package.as_ref().map(|p| p.meta.clone());
    sources
        .iter()
        .zip(assign_ids(&natural))
        .map(|(source, id)| {
            let meta = meta(source);
            let header = source
                .info
                .license
                .as_ref()
                .and_then(|l| l.text.header.clone());
            ExportPack {
                id,
                name: [source.info.name.clone(), source.folder_name.clone()]
                    .into_iter()
                    .find(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| "Pack".into()),
                version: meta
                    .as_ref()
                    .and_then(|m| m.version.clone())
                    .or_else(|| header?.version),
                publisher: meta.as_ref().and_then(|m| m.publisher.clone()),
                source_url: source
                    .edits
                    .source_url
                    .clone()
                    .or_else(|| meta.and_then(|m| m.source_url)),
                licence: licence(&source.info, &source.edits),
                origin: source.origin,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structure::{LicenseFile, LicenseText, PackageFile, PackageLicense, PackageMeta};

    fn ids(natural: &[&str]) -> Vec<String> {
        assign_ids(&natural.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn a_valid_package_name_is_the_id_else_the_folder_slugified() {
        assert_eq!(
            natural_id(Some("dungeon-props"), "Whatever"),
            "dungeon-props"
        );
        assert_eq!(
            natural_id(Some("Dungeon Props"), "Dungeon Props"),
            "dungeon-props"
        );
        assert_eq!(
            natural_id(Some("-bad"), "Stylarts  Desk_Set (v2)"),
            "stylarts-desk-set-v2"
        );
        assert_eq!(natural_id(None, "Kenney Crates"), "kenney-crates");
        assert_eq!(natural_id(None, "---"), "pack");
        assert_eq!(natural_id(None, "日本語アセット"), "pack");
    }

    #[test]
    fn clashing_ids_take_the_lowest_free_suffix() {
        assert_eq!(ids(&["foo", "foo", "foo-2"]), ["foo", "foo-3", "foo-2"]);
        assert_eq!(ids(&["pack", "pack"]), ["pack", "pack-2"]);
        assert_eq!(ids(&["a", "a", "a"]), ["a", "a-2", "a-3"]);
        assert_eq!(ids(&["a-2", "a", "a"]), ["a-2", "a", "a-3"]);
    }

    #[test]
    fn spdx_ids_are_recognised() {
        for id in [
            "CC0-1.0",
            "cc-by-4.0",
            "CC-BY-NC-4.0",
            "MIT",
            "BSD-3-Clause",
            "LicenseRef-Stylarts-EULA",
        ] {
            assert!(is_spdx_id(id), "{id}");
        }
        for id in [
            "CC0",
            "Proprietary",
            "Royalty free",
            "LicenseRef-",
            "LicenseRef-a b",
        ] {
            assert!(!is_spdx_id(id), "{id}");
        }
    }

    fn package(licenses: Vec<PackageLicense>) -> PackInfo {
        PackInfo {
            package: Some(PackageFile {
                path: "p/datapackage.json".into(),
                license: licenses.first().cloned(),
                meta: PackageMeta {
                    licenses,
                    ..Default::default()
                },
                parser: 2,
                size_bytes: 0,
                modified_at: 0,
            }),
            ..Default::default()
        }
    }

    fn license_file(kind: LicenseKind) -> PackInfo {
        PackInfo {
            license: Some(LicenseFile {
                path: "p/License.txt".into(),
                text: LicenseText { kind, header: None },
                parser: 1,
                size_bytes: 0,
                modified_at: 0,
            }),
            ..Default::default()
        }
    }

    fn yours(text: &str) -> PackOverride {
        PackOverride {
            license: Some(text.into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_license_ref_is_kept_with_its_title_and_url() {
        let info = package(vec![PackageLicense {
            kind: LicenseKind::Unknown,
            id: Some("LicenseRef-Stylarts-EULA".into()),
            title: Some("Stylarts EULA".into()),
            file_path: None,
            url: Some("https://example.com/eula".into()),
        }]);

        assert_eq!(
            licence(&info, &PackOverride::default()),
            Licence {
                spdx: Some("LicenseRef-Stylarts-EULA".into()),
                name: "Stylarts EULA".into(),
                url: Some("https://example.com/eula".into()),
            }
        );
    }

    #[test]
    fn the_license_files_kind_gives_its_id_when_it_has_one_version() {
        let cc0 = licence(&license_file(LicenseKind::Cc0), &PackOverride::default());
        assert_eq!(
            (cc0.spdx.as_deref(), cc0.name.as_str()),
            (Some("CC0-1.0"), "CC0")
        );
        let by = licence(&license_file(LicenseKind::CcBy), &PackOverride::default());
        assert_eq!((by.spdx, by.name.as_str()), (None, "CC-BY"));
    }

    #[test]
    fn your_text_names_the_licence_and_is_its_id_only_when_it_is_one() {
        let none = PackInfo::default();
        let mit = licence(&none, &yours("MIT"));
        assert_eq!(
            (mit.spdx.as_deref(), mit.name.as_str()),
            (Some("MIT"), "MIT")
        );
        let free = licence(&none, &yours("Bought on the store"));
        assert_eq!(
            (free.spdx, free.name.as_str()),
            (None, "Bought on the store")
        );
        // A stated SPDX id comes first (D15)
        let stated = package(vec![PackageLicense {
            kind: LicenseKind::Cc0,
            id: Some("CC0-1.0".into()),
            ..Default::default()
        }]);
        let both = licence(&stated, &yours("MIT"));
        assert_eq!(
            (both.spdx.as_deref(), both.name.as_str()),
            (Some("CC0-1.0"), "MIT")
        );
    }

    #[test]
    fn no_licence_at_all_is_unknown_with_a_null_id() {
        assert_eq!(
            licence(&PackInfo::default(), &PackOverride::default()),
            Licence {
                spdx: None,
                name: "Unknown".into(),
                url: None
            }
        );
        let unknown = licence(
            &license_file(LicenseKind::Unknown),
            &PackOverride::default(),
        );
        assert_eq!((unknown.spdx, unknown.name.as_str()), (None, "Unknown"));
    }

    #[test]
    fn origin_is_unity_with_a_meta_file_and_unreal_under_an_uasset_conversion() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("Unity/Textures")).unwrap();
        std::fs::write(root.join("Unity/Textures/moss.png"), "").unwrap();
        std::fs::write(root.join("Unity/Textures/moss.png.meta"), "").unwrap();
        std::fs::create_dir_all(root.join("Converted/Ruins")).unwrap();
        std::fs::write(
            root.join("Converted/conversion.json"),
            r#"{"contractVersion": 1, "steps": [{"tool": "UE", "toolVersion": "5", "from": "uasset"}]}"#,
        )
        .unwrap();
        let mut walker = Walker::new(root);
        let files = |paths: &[&str]| paths.iter().map(|p| p.to_string()).collect::<Vec<_>>();

        assert_eq!(
            infer_origin(root, &files(&["Unity/Textures/moss.png"]), &mut walker),
            Origin::Unity
        );
        assert_eq!(
            infer_origin(root, &files(&["Converted/Ruins/wall.png"]), &mut walker),
            Origin::Unreal
        );
        assert_eq!(
            infer_origin(root, &files(&["Plain/rock.png"]), &mut walker),
            Origin::Generic
        );
        assert_eq!(Origin::Unreal.normal_convention(), Convention::DirectX);
        assert_eq!(Origin::Unity.normal_convention(), Convention::OpenGl);
    }
}
