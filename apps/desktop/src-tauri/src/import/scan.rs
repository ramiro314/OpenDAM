//! An original's scan (plan pack-import, Presets and candidate detection): before anything is
//! written, it proposes the original's candidate packs, each with its preset, default target,
//! default steps and a preview of the folder tree its build would produce.
//!
//! The preview runs the build's own extract and place code over the archive's index
//! (`unitypackage::list_unitypackage`, `list_variant`, `zipfile::list_zip`, `place::place_listed`),
//! so it reads no file body (a package's entries are read through, its `pathname` entries kept)
//! and writes nothing. It adds the `datapackage.json` the metadata step writes. It runs no conversion: it counts one GLB per model stem `unity_glb` would
//! convert.
//!
//! The rows of the plan's table it detects, each offered when it matches, so a zip matching
//! several is offered as each:
//! - a `.unitypackage` (Unity cache): one pack named after the file, whose publisher is the
//!   folder below `Asset Store*/` in its path;
//! - a zip holding `.unitypackage` files (Fab): one pack named after the zip, with every package
//!   and every pipeline-named top folder of one as a variant, URP ticked;
//! - a zip holding a `.uproject` (Fab Unreal): one pack named after the zip, refused when the
//!   project has a `Source/` folder. Only the Unreal Editor knows which `.uasset` is a mesh or an
//!   animation, so its tree is an estimate: a GLB for every `.uasset` below `Content/`, at most,
//!   with the project's docs, placed by the pack's place steps;
//! - a zip whose folders hold `License*.txt` files (the Kenney All-in-1): one pack per folder
//!   directly holding one, at its path below a top folder every file shares, unticked under
//!   `Archive` and `Early access`;
//! - any zip, as one pack ("Any other zip").
//!
//! The engine checks each default target against the library (`engine::Engine::scan`).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;
use serde_json::json;

use super::metadata;
use super::place::{place_listed, PlaceStep};
use super::records::StepRecord;
use super::unitypackage::{
    below, list_unitypackage, list_variant, ExtractedFile, Extraction, SkippedEntry,
};
use super::unreal_glb::Project;
use super::zipfile::{index_zip, ZipIndex};

/// What scanning an original found.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Scan {
    /// The original, absolute.
    pub original_path: String,
    /// `unitypackage` or `zip`.
    pub kind: &'static str,
    /// One per row of the plan's table the original matches. You import one of them.
    pub offers: Vec<Offer>,
    /// Why a step kind can't run here, by kind: `unity_glb` without an accepted Blender, whose
    /// steps are then off, as an import records them.
    pub unavailable: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Offer {
    /// The row: `unity` (a cached `.unitypackage`), `fab`, `unreal`, `kenney` or `zip` (any
    /// other zip).
    pub layout: &'static str,
    pub candidates: Vec<Candidate>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub name: String,
    pub ticked: bool,
    /// The metadata preset: `unity`, `fab`, `kenney` or `none`.
    pub preset: &'static str,
    pub publisher: String,
    /// The default target, relative to the library root.
    pub target: String,
    /// Whether the default target is `<Publisher>/<Pack>`, so it moves when you change the
    /// publisher (Unity and Fab), or stays as it is (Kenney's `<Category>/<Pack>`, a zip's `<Pack>`).
    pub target_follows_publisher: bool,
    /// Why the library refuses the target, if it does: it exists, or nests with another pack's.
    pub target_error: Option<String>,
    /// A Fab pack's variants, else none. At most one is ticked.
    pub variants: Vec<Variant>,
    /// An Unreal pack's `.uproject`, by its path in the zip.
    pub project: Option<String>,
    /// The default steps, the extract step naming the ticked variant's source.
    pub steps: Vec<StepRecord>,
    /// The build's tree from the ticked source, or `None` while a Fab pack has no variant ticked.
    pub tree: Option<Tree>,
}

/// A Fab pack's source: a `.unitypackage` in the zip, or one pipeline folder of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Variant {
    /// The package's path in the zip.
    pub variant: String,
    /// The pipeline folder in the package, or `None` for all of it.
    pub subpath: Option<String>,
    /// The render pipeline its name names: `urp`, `built-in` or `hdrp`.
    pub pipeline: Option<&'static str>,
    pub ticked: bool,
    pub tree: Tree,
}

/// The folder tree a build would produce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tree {
    /// Every folder with a file in it or below it, the pack root (`""`) first, sorted by path.
    pub folders: Vec<FolderCount>,
    /// Every file the build writes, `.meta` files included.
    pub files: u64,
    /// The GLBs the conversion would write.
    pub glbs: u64,
    /// The entries the extraction would skip.
    pub skipped: Vec<SkippedEntry>,
    /// Why the build would fail, when it would: an unreadable archive, a clash in a step, an
    /// Unreal project with a `Source/` folder.
    pub error: Option<String>,
    /// Whether the tree is an estimate: an Unreal pack's, whose `glbs` are at most so many.
    pub estimate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderCount {
    pub path: String,
    /// The files directly in it.
    pub files: u64,
    /// The files in it and below it.
    pub total: u64,
}

impl Tree {
    /// The tree of a build whose files are `paths`, relative to the pack root. With `convert`,
    /// it counts the GLBs `unity_glb` would write: one per `.fbx` or `.obj` stem in each folder
    /// that ships no `.glb`.
    pub fn of<'p>(paths: impl IntoIterator<Item = &'p str>, convert: bool) -> Tree {
        // Each folder's files directly in it, and in it or below it.
        let mut folders: BTreeMap<&str, (u64, u64)> = BTreeMap::from([("", (0, 0))]);
        let mut models: BTreeMap<&str, (bool, std::collections::BTreeSet<&str>)> = BTreeMap::new();
        let mut files = 0;
        for path in paths {
            files += 1;
            let (folder, name) = path.rsplit_once('/').unwrap_or(("", path));
            folders.entry(folder).or_default().0 += 1;
            for (i, _) in path.match_indices('/') {
                folders.entry(&path[..i]).or_default().1 += 1;
            }
            folders.get_mut("").unwrap().1 += 1;
            if let Some((stem, ext)) = name.rsplit_once('.').filter(|(s, _)| !s.is_empty()) {
                let entry = models.entry(folder).or_default();
                if ext.eq_ignore_ascii_case("glb") {
                    entry.0 = true;
                } else if ext.eq_ignore_ascii_case("fbx") || ext.eq_ignore_ascii_case("obj") {
                    entry.1.insert(stem);
                }
            }
        }
        let glbs = if convert {
            models
                .values()
                .filter(|(ships_glb, _)| !ships_glb)
                .map(|(_, stems)| stems.len() as u64)
                .sum()
        } else {
            0
        };
        Tree {
            folders: folders
                .into_iter()
                .map(|(path, (files, total))| FolderCount {
                    path: path.to_string(),
                    files,
                    total,
                })
                .collect(),
            files,
            glbs,
            skipped: Vec::new(),
            error: None,
            estimate: false,
        }
    }

    fn failed(error: anyhow::Error, skipped: Vec<SkippedEntry>) -> Tree {
        Tree {
            skipped,
            error: Some(format!("{error:#}")),
            ..Tree::of([], false)
        }
    }
}

/// A source's listing, placed by `place`, with the `datapackage.json` the metadata step writes
/// at the pack root (every candidate's default steps end with it), and counted. A listing or
/// step that fails gives a tree naming the error, as the build would fail.
fn preview(listing: Result<Extraction>, place: &[PlaceStep], convert: bool) -> Tree {
    let mut extraction = match listing {
        Ok(extraction) => extraction,
        Err(err) => return Tree::failed(err, Vec::new()),
    };
    if let Err(err) = place_listed(&mut extraction.files, place) {
        return Tree::failed(err, extraction.skipped);
    }
    if let Some(path) = metadata::clash(&extraction.files) {
        let err = anyhow::anyhow!(metadata::clash_message(path));
        return Tree::failed(err, extraction.skipped);
    }
    let files = extraction.files.iter().map(|f| f.path.as_str());
    Tree {
        skipped: extraction.skipped,
        ..Tree::of(files.chain([metadata::FILE]), convert)
    }
}

/// The tree of one source of `original`, of kind `kind`, placed by `place`: what the import
/// dialog shows once you change a candidate's place steps. The source is a `.unitypackage`'s
/// `subpath`, or a zip's `variant` package (and its `subpath`), else the zip's `subpath`. A
/// package's tree counts the GLBs `unity_glb` would write, as `detect`'s do.
pub fn preview_source(
    original: &Path,
    kind: &str,
    variant: Option<&str>,
    subpath: Option<&str>,
    place: &[PlaceStep],
) -> Tree {
    let listing = match (kind, variant) {
        ("unitypackage", _) => list_unitypackage(original, subpath),
        (_, Some(variant)) => list_variant(original, variant, subpath),
        (_, None) => index_zip(original).and_then(|index| index.list(subpath)),
    };
    preview(listing, place, kind == "unitypackage" || variant.is_some())
}

/// The files of `extraction` below `folder`, relative to it, as a listing with that sub-folder
/// would give them, and every skipped entry. A package listed with a sub-folder reports every
/// skipped entry too, but a nested package's only when it lies in the sub-folder (plan Known
/// gaps: the preview may list a few more).
fn below_folder(extraction: &Extraction, folder: &str) -> Extraction {
    Extraction {
        files: extraction
            .files
            .iter()
            .filter_map(|f| {
                let rel = below(&f.path, folder).filter(|r| !r.is_empty())?;
                Some(ExtractedFile {
                    path: rel.to_string(),
                    ..f.clone()
                })
            })
            .collect(),
        skipped: extraction.skipped.clone(),
    }
}

/// The render pipeline a package's or folder's name names, read by its words:
/// `URP`/`Universal`, `Built-In`/`BiRP`/`Standard`, `HDRP`.
pub fn pipeline(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let has = |w: &str| words.contains(&w);
    if has("urp") || has("universal") {
        Some("urp")
    } else if has("hdrp") {
        Some("hdrp")
    } else if has("birp")
        || has("standard")
        || has("builtin")
        || words.windows(2).any(|p| p == ["built", "in"])
    {
        Some("built-in")
    } else {
        None
    }
}

fn place_steps(steps: &[PlaceStep]) -> impl Iterator<Item = StepRecord> + '_ {
    steps.iter().map(StepRecord::place)
}

fn step(kind: &str, options: serde_json::Value) -> StepRecord {
    StepRecord::new(kind, options)
}

/// The source options of an extract step: the non-empty ones of `variant` and `subpath`.
fn source(variant: Option<&str>, subpath: Option<&str>) -> serde_json::Value {
    let mut options = serde_json::Map::new();
    if let Some(variant) = variant {
        options.insert("variant".into(), variant.into());
    }
    if let Some(subpath) = subpath {
        options.insert("subpath".into(), subpath.into());
    }
    options.into()
}

fn unity_glb() -> StepRecord {
    step("unity_glb", super::unity_glb::Options::default_json())
}

fn metadata(preset: &str, publisher: &str) -> StepRecord {
    step(
        "metadata",
        json!({ "preset": preset, "publisher": publisher }),
    )
}

/// `<Publisher>/<Pack>`, or `<Pack>` while no publisher is known.
fn publisher_target(publisher: &str, name: &str) -> String {
    if publisher.is_empty() {
        name.to_string()
    } else {
        format!("{publisher}/{name}")
    }
}

/// An original's file name without its extension.
fn stem(original: &Path) -> String {
    original
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The folder below a Unity cache folder (`Asset Store-5.x/<Publisher>/<Category>/`) in the
/// package's path.
fn cache_publisher(package: &Path) -> Option<String> {
    let parts: Vec<String> = package
        .parent()?
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let cache = parts
        .iter()
        .rposition(|p| p.to_ascii_lowercase().starts_with("asset store"))?;
    parts.get(cache + 1).filter(|p| !p.is_empty()).cloned()
}

/// Detects the candidate packs of `original`, of kind `kind` (`unitypackage` or `zip`).
/// `publisher` is the default publisher, the last one used in the library. Fails only when the
/// original can't be read as its kind.
pub fn detect(original: &Path, kind: &str, publisher: &str) -> Result<Vec<Offer>> {
    let name = stem(original);
    if kind == "unitypackage" {
        let publisher = cache_publisher(original).unwrap_or_else(|| publisher.to_string());
        let place = [PlaceStep::Flatten];
        let listing = list_unitypackage(original, None);
        let mut steps = vec![step("extract_unitypackage", source(None, None))];
        steps.extend(place_steps(&place));
        steps.extend([unity_glb(), metadata("unity", &publisher)]);
        return Ok(vec![Offer {
            layout: "unity",
            candidates: vec![Candidate {
                target: publisher_target(&publisher, &name),
                tree: Some(preview(listing, &place, true)),
                name,
                ticked: true,
                preset: "unity",
                publisher,
                target_error: None,
                target_follows_publisher: true,
                variants: Vec::new(),
                project: None,
                steps,
            }],
        }]);
    }

    let index = index_zip(original)?;
    let zip = index.list(None)?;
    let mut offers = Vec::new();
    if let Some(fab) = fab(original, &zip, &name, publisher) {
        offers.push(fab);
    }
    if let Some(unreal) = unreal(&index, &zip, &name, publisher) {
        offers.push(unreal);
    }
    if let Some(kenney) = kenney(&index, &zip, &name) {
        offers.push(kenney);
    }
    let place = [PlaceStep::Flatten];
    let mut steps = vec![step("extract_zip", source(None, None))];
    steps.extend(place_steps(&place));
    steps.push(metadata("none", publisher));
    offers.push(Offer {
        layout: "zip",
        candidates: vec![Candidate {
            target: name.clone(),
            tree: Some(preview(Ok(zip), &place, false)),
            name,
            ticked: true,
            preset: "none",
            publisher: publisher.to_string(),
            target_error: None,
            target_follows_publisher: false,
            variants: Vec::new(),
            project: None,
            steps,
        }],
    });
    Ok(offers)
}

/// A zip holding `.unitypackage` files, at any depth: one pack, every package a variant, and so
/// is each top folder of one whose name names a pipeline. A URP variant is ticked, else a lone
/// one.
fn fab(original: &Path, zip: &Extraction, name: &str, publisher: &str) -> Option<Offer> {
    let place = [
        PlaceStep::Flatten,
        PlaceStep::Hoist {
            folder: "Art".into(),
        },
    ];
    let mut variants = Vec::new();
    for package in zip.files.iter().filter(|f| {
        f.path
            .rsplit_once('.')
            .is_some_and(|(stem, ext)| !stem.is_empty() && ext.eq_ignore_ascii_case("unitypackage"))
    }) {
        let in_zip = package.source_path[0].as_str();
        let file_name = package.path.rsplit('/').next().unwrap_or(&package.path);
        let listing = list_variant(original, in_zip, None);
        let folders: Vec<(String, &'static str)> = match &listing {
            Ok(listing) => {
                let mut tops: Vec<&str> = listing
                    .files
                    .iter()
                    .filter_map(|f| Some(f.path.split_once('/')?.0))
                    .collect();
                tops.dedup();
                tops.into_iter()
                    .filter_map(|top| Some((top.to_string(), pipeline(top)?)))
                    .collect()
            }
            Err(_) => Vec::new(),
        };
        let folder_trees: Vec<Tree> = match &listing {
            Ok(listing) => folders
                .iter()
                .map(|(top, _)| preview(Ok(below_folder(listing, top)), &place, true))
                .collect(),
            Err(_) => Vec::new(),
        };
        variants.push(Variant {
            variant: in_zip.to_string(),
            subpath: None,
            pipeline: pipeline(file_name.rsplit_once('.').map_or(file_name, |(s, _)| s)),
            ticked: false,
            tree: preview(listing, &place, true),
        });
        for ((top, pipeline), tree) in folders.into_iter().zip(folder_trees) {
            variants.push(Variant {
                variant: in_zip.to_string(),
                subpath: Some(top),
                pipeline: Some(pipeline),
                ticked: false,
                tree,
            });
        }
    }
    if variants.is_empty() {
        return None;
    }
    let ticked = variants
        .iter()
        .position(|v| v.pipeline == Some("urp"))
        .or((variants.len() == 1).then_some(0));
    let mut extract = source(None, None);
    let mut tree = None;
    if let Some(i) = ticked {
        let variant = &mut variants[i];
        variant.ticked = true;
        extract = source(Some(&variant.variant), variant.subpath.as_deref());
        tree = Some(variant.tree.clone());
    }
    let mut steps = vec![step("extract_unitypackage", extract)];
    steps.extend(place_steps(&place));
    steps.extend([unity_glb(), metadata("fab", publisher)]);
    Some(Offer {
        layout: "fab",
        candidates: vec![Candidate {
            name: name.to_string(),
            ticked: true,
            preset: "fab",
            publisher: publisher.to_string(),
            target: publisher_target(publisher, name),
            target_error: None,
            target_follows_publisher: true,
            variants,
            project: None,
            steps,
            tree,
        }],
    })
}

/// The place steps of an Unreal pack: its meshes hoisted to the root (`Waterways/Meshes/`).
pub fn unreal_place() -> Vec<PlaceStep> {
    vec![
        PlaceStep::Flatten,
        PlaceStep::Hoist {
            folder: "Meshes".into(),
        },
    ]
}

/// A zip holding a `.uproject`, at any depth: one pack named after the zip (`Waterways`, not
/// `Feif`), unticked with the reason when the project can't be exported. Its tree is the
/// project's estimate (`Project::estimate`), placed and counted as a build's would be.
fn unreal(index: &ZipIndex, zip: &Extraction, name: &str, publisher: &str) -> Option<Offer> {
    let place = unreal_place();
    let found = Project::find(zip).transpose()?;
    let (project, tree) = match found {
        Ok(project) => {
            // The entries an export would skip are those of the project's folder.
            let skipped = index
                .list(project.dir.as_deref())
                .map(|listing| listing.skipped)
                .unwrap_or_default();
            let estimate = Extraction {
                files: project.estimate(),
                skipped,
            };
            let mut tree = preview(Ok(estimate), &place, false);
            if tree.error.is_none() {
                // At most one per `.uasset`: "at most N models".
                tree.glbs = project.assets.len() as u64;
            }
            tree.error = project.refusal().or(tree.error);
            tree.estimate = true;
            (Some(project), tree)
        }
        Err(error) => (None, Tree::failed(anyhow::anyhow!(error), Vec::new())),
    };
    let mut steps = vec![step(
        "export_unreal",
        super::unreal_glb::Options::default_json(),
    )];
    steps.extend(place_steps(&place));
    steps.push(metadata("fab", publisher));
    Some(Offer {
        layout: "unreal",
        candidates: vec![Candidate {
            name: name.to_string(),
            ticked: tree.error.is_none(),
            preset: "fab",
            publisher: publisher.to_string(),
            target: publisher_target(publisher, name),
            target_error: None,
            variants: Vec::new(),
            project: project.map(|p| p.path()),
            steps,
            tree: Some(tree),
        }],
    })
}

/// Categories whose packs are listed unticked.
const UNTICKED: [&str; 2] = ["archive", "early access"];

/// A zip whose folders hold `License*.txt` files: one pack per folder directly holding one, the
/// outermost where they nest, at its path below a top folder every file shares (so
/// `<Category>/<Pack>` in the Kenney All-in-1). `zip` is the whole zip's listing from `index`,
/// and each pack's preview lists its folder from `index` as its build's `extract_zip` would,
/// without reading the zip again.
fn kenney(index: &ZipIndex, zip: &Extraction, name: &str) -> Option<Offer> {
    let mut packs: Vec<&str> = zip
        .files
        .iter()
        .filter(|f| {
            let file = f
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&f.path)
                .to_ascii_lowercase();
            file.starts_with("license") && file.ends_with(".txt")
        })
        .map(|f| f.path.rsplit_once('/').map_or("", |(folder, _)| folder))
        .collect();
    packs.sort();
    packs.dedup();
    let outer = packs.clone();
    packs.retain(|p| {
        !outer
            .iter()
            .any(|o| o != p && (o.is_empty() || below(p, o).is_some()))
    });
    if packs.is_empty() {
        return None;
    }

    let first = zip.files.first().and_then(|f| f.path.split_once('/'));
    let top = first.map(|(top, _)| top).filter(|top| {
        zip.files
            .iter()
            .all(|f| f.path.split_once('/').is_some_and(|(t, _)| t == *top))
    });

    let candidates = packs
        .into_iter()
        .map(|folder| {
            let rel = match top {
                Some(top) => below(folder, top).unwrap_or(folder),
                None => folder,
            };
            let pack = rel
                .rsplit('/')
                .next()
                .filter(|p| !p.is_empty())
                .or(top.filter(|_| rel.is_empty()))
                .unwrap_or(name);
            let target = if rel.is_empty() { pack } else { rel };
            let ticked = !rel
                .split('/')
                .rev()
                .skip(1)
                .any(|c| UNTICKED.contains(&c.to_ascii_lowercase().as_str()));
            let subpath = (!folder.is_empty()).then_some(folder);
            let files = match subpath {
                Some(folder) => index.list(Some(folder)),
                None => Ok(Extraction {
                    files: zip.files.clone(),
                    skipped: zip.skipped.clone(),
                }),
            };
            Candidate {
                name: pack.to_string(),
                ticked,
                preset: "kenney",
                publisher: "Kenney".into(),
                target: target.to_string(),
                target_error: None,
                target_follows_publisher: false,
                variants: Vec::new(),
                project: None,
                steps: vec![
                    step("extract_zip", source(None, subpath)),
                    step(
                        "metadata",
                        json!({ "preset": "kenney", "publisher": "Kenney",
                                "lookup_store_page": true }),
                    ),
                ],
                tree: Some(preview(files, &[], false)),
            }
        })
        .collect();
    Some(Offer {
        layout: "kenney",
        candidates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::place::place;
    use crate::import::unitypackage::tests::{fixture, on_disk};
    use crate::import::unitypackage::{extract_unitypackage, extract_variant};
    use crate::import::zipfile::{extract_zip, list_zip};

    fn flatten() -> Vec<PlaceStep> {
        vec![PlaceStep::Flatten]
    }

    fn flatten_hoist_art() -> Vec<PlaceStep> {
        vec![
            PlaceStep::Flatten,
            PlaceStep::Hoist {
                folder: "Art".into(),
            },
        ]
    }

    /// The tree a build makes on disk: `extract` into a fresh folder, then `steps`, then the
    /// metadata step's `datapackage.json` (its content is the metadata tests' to check).
    fn built(
        extract: impl FnOnce(&Path) -> Result<Extraction>,
        steps: &[PlaceStep],
        convert: bool,
    ) -> Tree {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("Pack");
        let mut extraction = extract(&root).unwrap();
        place(&root, &mut extraction.files, steps).unwrap();
        std::fs::write(root.join(metadata::FILE), "{}").unwrap();
        Tree {
            skipped: extraction.skipped,
            ..Tree::of(on_disk(&root).iter().map(String::as_str), convert)
        }
    }

    fn kinds(steps: &[StepRecord]) -> Vec<(&str, serde_json::Value)> {
        steps
            .iter()
            .map(|s| (s.kind.as_str(), s.options.clone()))
            .collect()
    }

    fn layouts(offers: &[Offer]) -> Vec<&str> {
        offers.iter().map(|o| o.layout).collect()
    }

    #[test]
    fn a_unitypackage_is_one_unity_pack_whose_tree_is_its_build() {
        let original = fixture("synthetic.unitypackage");
        let offers = detect(&original, "unitypackage", "Last Used").unwrap();
        assert_eq!(layouts(&offers), ["unity"]);
        let [pack] = &offers[0].candidates[..] else {
            panic!("{offers:?}");
        };
        assert_eq!(
            (pack.name.as_str(), pack.ticked, pack.preset),
            ("synthetic", true, "unity")
        );
        // Outside the Unity cache, the publisher is the last one used.
        assert_eq!(pack.publisher, "Last Used");
        assert_eq!(pack.target, "Last Used/synthetic");
        assert!(pack.variants.is_empty());
        assert_eq!(
            kinds(&pack.steps),
            [
                ("extract_unitypackage", json!({})),
                ("flatten", json!({})),
                ("unity_glb", json!({ "max_texture": 1024 })),
                (
                    "metadata",
                    json!({ "preset": "unity", "publisher": "Last Used" })
                ),
            ]
        );
        let expected = built(
            |to| extract_unitypackage(&original, to, None),
            &flatten(),
            true,
        );
        let tree = pack.tree.as_ref().unwrap();
        assert_eq!(tree, &expected);
        // 16 extracted files and the metadata step's `datapackage.json`.
        assert_eq!((tree.files, tree.glbs), (17, 1), "{tree:?}");
        assert_eq!(
            tree.folders.first(),
            Some(&FolderCount {
                path: String::new(),
                files: 3,
                total: 17,
            })
        );
        assert!(tree
            .folders
            .iter()
            .any(|f| f.path == "Art/Models" && f.files == 2 && f.total == 2));
    }

    #[test]
    fn a_cached_package_takes_its_publisher_from_its_path() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp
            .path()
            .join("Asset Store-5.x/Leartes Studios/3D ModelsEnvironments");
        std::fs::create_dir_all(&folder).unwrap();
        let original = folder.join("Ancient Ruins.unitypackage");
        std::fs::copy(fixture("synthetic.unitypackage"), &original).unwrap();

        for last in ["", "Someone Else"] {
            let offers = detect(&original, "unitypackage", last).unwrap();
            let pack = &offers[0].candidates[0];
            assert_eq!(pack.name, "Ancient Ruins");
            assert_eq!(pack.publisher, "Leartes Studios");
            assert_eq!(pack.target, "Leartes Studios/Ancient Ruins");
            assert_eq!(
                pack.steps.last().unwrap().options,
                json!({ "preset": "unity", "publisher": "Leartes Studios" })
            );
        }
        // With no publisher known, the target is the pack alone.
        let offers = detect(&fixture("synthetic.unitypackage"), "unitypackage", "").unwrap();
        assert_eq!(offers[0].candidates[0].target, "synthetic");
    }

    #[test]
    fn a_fab_zip_offers_each_variant_package_with_urp_ticked() {
        let original = fixture("fab-unity.zip");
        let offers = detect(&original, "zip", "Stylarts").unwrap();
        assert_eq!(layouts(&offers), ["fab", "zip"]);
        let [pack] = &offers[0].candidates[..] else {
            panic!("{offers:?}");
        };
        assert_eq!(
            (pack.name.as_str(), pack.preset, pack.target.as_str()),
            ("fab-unity", "fab", "Stylarts/fab-unity")
        );
        let variants: Vec<_> = pack
            .variants
            .iter()
            .map(|v| {
                (
                    v.variant.as_str(),
                    v.subpath.as_deref(),
                    v.pipeline,
                    v.ticked,
                )
            })
            .collect();
        assert_eq!(
            variants,
            [
                (
                    "Crypt/Crypt_Built-In.unitypackage",
                    None,
                    Some("built-in"),
                    false
                ),
                ("Crypt/Crypt_URP.unitypackage", None, Some("urp"), true),
            ]
        );
        assert_eq!(
            kinds(&pack.steps),
            [
                (
                    "extract_unitypackage",
                    json!({ "variant": "Crypt/Crypt_URP.unitypackage" })
                ),
                ("flatten", json!({})),
                ("hoist", json!({ "folder": "Art" })),
                ("unity_glb", json!({ "max_texture": 1024 })),
                (
                    "metadata",
                    json!({ "preset": "fab", "publisher": "Stylarts" })
                ),
            ]
        );
        for variant in &pack.variants {
            let expected = built(
                |to| extract_variant(&original, &variant.variant, to, None),
                &flatten_hoist_art(),
                true,
            );
            assert_eq!(variant.tree, expected, "{}", variant.variant);
        }
        assert_eq!(pack.tree.as_ref(), Some(&pack.variants[1].tree));
    }

    #[test]
    fn a_fab_package_with_pipeline_folders_offers_each_folder() {
        let original = fixture("fab-unity-folders.zip");
        let offers = detect(&original, "zip", "Stylarts").unwrap();
        assert_eq!(layouts(&offers), ["fab", "zip"]);
        let pack = &offers[0].candidates[0];
        let variants: Vec<_> = pack
            .variants
            .iter()
            .map(|v| (v.subpath.as_deref(), v.pipeline, v.ticked))
            .collect();
        assert_eq!(
            variants,
            [
                (None, None, false),
                (Some("Built-In"), Some("built-in"), false),
                (Some("URP"), Some("urp"), true),
            ]
        );
        assert!(pack
            .variants
            .iter()
            .all(|v| v.variant == "Crypt/Crypt.unitypackage"));
        assert_eq!(
            pack.steps[0].options,
            json!({ "variant": "Crypt/Crypt.unitypackage", "subpath": "URP" })
        );
        for variant in &pack.variants {
            let expected = built(
                |to| extract_variant(&original, &variant.variant, to, variant.subpath.as_deref()),
                &flatten_hoist_art(),
                true,
            );
            assert_eq!(variant.tree, expected, "{:?}", variant.subpath);
        }
        assert_eq!(pack.tree.as_ref(), Some(&pack.variants[2].tree));
    }

    #[test]
    fn a_lone_variant_is_ticked_and_none_is_among_several_without_urp() {
        let tmp = tempfile::tempdir().unwrap();
        let package = std::fs::read(fixture("synthetic.unitypackage")).unwrap();
        let zip_of = |name: &str, entries: &[&str]| {
            let path = tmp.path().join(name);
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
            for entry in entries {
                zip.start_file(*entry, zip::write::SimpleFileOptions::default())
                    .unwrap();
                std::io::Write::write_all(&mut zip, &package).unwrap();
            }
            zip.finish().unwrap();
            path
        };

        let lone = zip_of("lone.zip", &["Deep/Down/Pack.unitypackage"]);
        let offers = detect(&lone, "zip", "").unwrap();
        let pack = &offers[0].candidates[0];
        // The package's own `Built-In/` wrapper names a pipeline, so it is a variant too.
        let ticked: Vec<_> = pack
            .variants
            .iter()
            .map(|v| (v.subpath.as_deref(), v.ticked))
            .collect();
        assert_eq!(ticked, [(None, false), (Some("Built-In"), false)]);
        assert!(pack.tree.is_none());

        let two = zip_of(
            "two.zip",
            &["A_HDRP.unitypackage", "B_Standard.unitypackage"],
        );
        let offers = detect(&two, "zip", "").unwrap();
        let pack = &offers[0].candidates[0];
        assert!(pack.variants.iter().all(|v| !v.ticked));
        assert_eq!(pack.steps[0].options, json!({}));
        assert!(pack.tree.is_none());

        let single = tmp.path().join("single.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&single).unwrap());
        zip.start_file(
            "Only.unitypackage",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        std::io::Write::write_all(
            &mut zip,
            &std::fs::read(fixture("escape.unitypackage")).unwrap(),
        )
        .unwrap();
        zip.finish().unwrap();
        let offers = detect(&single, "zip", "").unwrap();
        let pack = &offers[0].candidates[0];
        assert_eq!(pack.variants.len(), 1);
        assert!(pack.variants[0].ticked);
        assert_eq!(
            pack.steps[0].options,
            json!({ "variant": "Only.unitypackage" })
        );
        assert_eq!(pack.tree.as_ref(), Some(&pack.variants[0].tree));
    }

    #[test]
    fn the_kenney_bundle_offers_a_pack_per_license_folder_under_its_category() {
        let original = fixture("kenney.zip");
        let offers = detect(&original, "zip", "Someone").unwrap();
        assert_eq!(layouts(&offers), ["kenney", "zip"]);
        let packs: Vec<_> = offers[0]
            .candidates
            .iter()
            .map(|c| (c.name.as_str(), c.target.as_str(), c.ticked, c.preset))
            .collect();
        assert_eq!(
            packs,
            [
                ("Fixture Tiles", "2D assets/Fixture Tiles", true, "kenney"),
                (
                    "Fixture Car Kit",
                    "3D assets/Fixture Car Kit",
                    true,
                    "kenney"
                ),
                (
                    "Fixture Old Kit",
                    "Archive/Fixture Old Kit",
                    false,
                    "kenney"
                ),
            ]
        );
        for pack in &offers[0].candidates {
            let subpath = format!("Kenney/{}", pack.target);
            assert_eq!(pack.publisher, "Kenney");
            assert_eq!(
                kinds(&pack.steps),
                [
                    ("extract_zip", json!({ "subpath": subpath })),
                    (
                        "metadata",
                        json!({ "preset": "kenney", "publisher": "Kenney",
                                "lookup_store_page": true })
                    ),
                ]
            );
            let expected = built(|to| extract_zip(&original, to, Some(&subpath)), &[], false);
            assert_eq!(pack.tree.as_ref(), Some(&expected), "{}", pack.name);
        }
        let car = offers[0].candidates[1].tree.as_ref().unwrap();
        assert_eq!((car.files, car.glbs), (4, 0));

        // Offered as any other zip too.
        let [zip] = &offers[1].candidates[..] else {
            panic!("{offers:?}");
        };
        assert_eq!(
            (
                zip.name.as_str(),
                zip.target.as_str(),
                zip.preset,
                zip.ticked
            ),
            ("kenney", "kenney", "none", true)
        );
        assert_eq!(
            kinds(&zip.steps),
            [
                ("extract_zip", json!({})),
                ("flatten", json!({})),
                (
                    "metadata",
                    json!({ "preset": "none", "publisher": "Someone" })
                ),
            ]
        );
        let expected = built(|to| extract_zip(&original, to, None), &flatten(), false);
        assert_eq!(zip.tree.as_ref(), Some(&expected));
    }

    #[test]
    fn an_unreal_zip_is_one_pack_whose_tree_estimates_the_export_placed() {
        let offers = detect(&fixture("unreal.zip"), "zip", "Fab").unwrap();
        assert_eq!(layouts(&offers), ["unreal", "zip"]);
        let [pack] = &offers[0].candidates[..] else {
            panic!("{offers:?}");
        };
        // Named after the zip, not the project.
        assert_eq!(
            (
                pack.name.as_str(),
                pack.preset,
                pack.target.as_str(),
                pack.ticked
            ),
            ("unreal", "fab", "Fab/unreal", true)
        );
        assert_eq!(pack.project.as_deref(), Some("Feif/Feif.uproject"));
        assert_eq!(
            kinds(&pack.steps),
            [
                ("export_unreal", json!({ "batch": 30 })),
                ("flatten", json!({})),
                ("hoist", json!({ "folder": "Meshes" })),
                ("metadata", json!({ "preset": "fab", "publisher": "Fab" })),
            ]
        );
        let tree = pack.tree.as_ref().unwrap();
        assert!(tree.estimate);
        assert_eq!(tree.error, None);
        // At most a GLB per .uasset (the level is none), placed: `flatten` merges `Fixture/`,
        // and `hoist Meshes` lifts the meshes beside the docs and `datapackage.json`.
        assert_eq!((tree.files, tree.glbs), (7, 4));
        let folders: Vec<_> = tree
            .folders
            .iter()
            .map(|f| (f.path.as_str(), f.files, f.total))
            .collect();
        assert_eq!(folders, [("", 6, 7), ("Textures", 1, 1)]);
        let expected = Tree {
            glbs: 4,
            estimate: true,
            ..Tree::of(
                [
                    "README.md",
                    "Readme.txt",
                    "SM_A.glb",
                    "SM_B.glb",
                    "SM_C.glb",
                    "Textures/T_A.glb",
                    metadata::FILE,
                ],
                false,
            )
        };
        assert_eq!(tree, &expected);
    }

    #[test]
    fn license_folders_nest_into_the_outermost_and_a_root_license_names_the_pack() {
        let zip = |paths: &[&str]| Extraction {
            files: paths
                .iter()
                .map(|p| ExtractedFile {
                    path: p.to_string(),
                    source_path: vec![p.to_string()],
                    size: 1,
                })
                .collect(),
            skipped: Vec::new(),
        };
        // Only the targets are checked here: the previews list another zip's folders.
        let index = index_zip(&fixture("escape.zip")).unwrap();
        let targets = |offer: Offer| -> Vec<(String, Option<String>, bool)> {
            offer
                .candidates
                .into_iter()
                .map(|c| {
                    let subpath = c.steps[0].options["subpath"].as_str().map(str::to_string);
                    (c.target, subpath, c.ticked)
                })
                .collect()
        };

        let nested = zip(&[
            "Top/Kit/LICENSE.txt",
            "Top/Kit/Sub/License (1).txt",
            "Top/Early Access/New/license.TXT",
            "Top/Other/a.png",
        ]);
        assert_eq!(
            targets(kenney(&index, &nested, "bundle").unwrap()),
            [
                (
                    "Early Access/New".into(),
                    Some("Top/Early Access/New".into()),
                    false
                ),
                ("Kit".into(), Some("Top/Kit".into()), true),
            ]
        );
        let single = zip(&["car-kit/License.txt", "car-kit/Models/car.glb"]);
        assert_eq!(
            targets(kenney(&index, &single, "kenney_car-kit").unwrap()),
            [("car-kit".into(), Some("car-kit".into()), true)]
        );
        let flat = zip(&["License.txt", "car.glb"]);
        assert_eq!(
            targets(kenney(&index, &flat, "kenney_car-kit").unwrap()),
            [("kenney_car-kit".into(), None, true)]
        );
        assert!(kenney(
            &index,
            &zip(&["Pack/Readme.txt", "Pack/LicenseNotes.md"]),
            "x"
        )
        .is_none());
    }

    #[test]
    fn skipped_entries_and_failing_steps_show_in_the_tree() {
        for name in ["escape.unitypackage", "escape.zip"] {
            let original = fixture(name);
            let kind = if name.ends_with(".zip") {
                "zip"
            } else {
                "unitypackage"
            };
            let offers = detect(&original, kind, "").unwrap();
            let pack = offers.last().unwrap().candidates.last().unwrap();
            let expected = if kind == "zip" {
                built(|to| extract_zip(&original, to, None), &flatten(), false)
            } else {
                built(
                    |to| extract_unitypackage(&original, to, None),
                    &flatten(),
                    true,
                )
            };
            let tree = pack.tree.as_ref().unwrap();
            assert_eq!(tree, &expected, "{name}");
            assert!(tree.skipped.len() >= 5, "{name}: {tree:?}");
        }

        // A clash fails the preview's tree as it would fail the build.
        let readme = "Assets/Pack/Readme.txt";
        let steps = [
            PlaceStep::Flatten,
            PlaceStep::Rename {
                source: vec![readme.into()],
                name: "T_Rock.png".into(),
            },
            PlaceStep::Hoist {
                folder: "Art".into(),
            },
        ];
        let listing = list_unitypackage(&fixture("synthetic.unitypackage"), None);
        let tree = preview(listing, &steps, true);
        assert_eq!(
            tree.error.as_deref(),
            Some("hoist Art: merging Art into the pack root would overwrite T_Rock.png, T_Rock.png.meta")
        );
        assert_eq!(tree.files, 0);

        // An original holding its own datapackage.json where the metadata step writes one fails
        // the preview as it fails the build.
        let tmp = tempfile::tempdir().unwrap();
        let own = tmp.path().join("own.zip");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&own).unwrap());
        for name in ["Pack/DataPackage.json", "Pack/rock.fbx"] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
        }
        zip.finish().unwrap();
        let offers = detect(&own, "zip", "").unwrap();
        let tree = offers[0].candidates[0].tree.as_ref().unwrap();
        assert_eq!(
            tree.error.as_deref(),
            Some(metadata::clash_message("DataPackage.json").as_str())
        );

        // An original that isn't what its name says fails the scan.
        let bogus = tmp.path().join("bogus.zip");
        std::fs::write(&bogus, b"not a zip").unwrap();
        assert!(detect(&bogus, "zip", "").is_err());
        // A package that can't be read is a tree naming why.
        let bogus = tmp.path().join("bogus.unitypackage");
        std::fs::write(&bogus, b"not a package").unwrap();
        let offers = detect(&bogus, "unitypackage", "").unwrap();
        assert!(offers[0].candidates[0]
            .tree
            .as_ref()
            .unwrap()
            .error
            .is_some());
    }

    #[test]
    fn a_listing_names_exactly_what_the_extraction_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let synthetic = fixture("synthetic.unitypackage");
        for subpath in [
            None,
            Some("Built-In"),
            Some("Built-In/Built-In_Pack/Pack/Houses"),
        ] {
            let to = tmp.path().join(format!("u{}", subpath.unwrap_or("").len()));
            let written = extract_unitypackage(&synthetic, &to, subpath).unwrap();
            let listed = list_unitypackage(&synthetic, subpath).unwrap();
            assert_eq!(listed.files, written.files, "{subpath:?}");
            assert_eq!(listed.skipped, written.skipped, "{subpath:?}");
        }
        let escape = fixture("escape.unitypackage");
        let written = extract_unitypackage(&escape, &tmp.path().join("e"), None).unwrap();
        let listed = list_unitypackage(&escape, None).unwrap();
        assert_eq!(
            (listed.files, listed.skipped),
            (written.files, written.skipped)
        );

        let fab = fixture("fab-unity-folders.zip");
        let variant = "Crypt/Crypt.unitypackage";
        let written = extract_variant(&fab, variant, &tmp.path().join("f"), Some("URP")).unwrap();
        let listed = list_variant(&fab, variant, Some("URP")).unwrap();
        assert_eq!(listed.files, written.files);
        assert_eq!(listed.skipped, written.skipped);
        let whole = list_variant(&fab, variant, None).unwrap();
        assert_eq!(below_folder(&whole, "URP").files, written.files);

        for (name, subpath) in [
            ("kenney.zip", None),
            ("kenney.zip", Some("Kenney/3D assets")),
            ("escape.zip", None),
            ("escape.zip", Some("Esc")),
        ] {
            let to = tmp.path().join(format!("{name}{}", subpath.unwrap_or("")));
            let written = extract_zip(&fixture(name), &to, subpath).unwrap();
            let listed = list_zip(&fixture(name), subpath).unwrap();
            assert_eq!(listed.files, written.files, "{name} {subpath:?}");
            assert_eq!(listed.skipped, written.skipped, "{name} {subpath:?}");
        }
        // Errors match too: no file under the sub-folder.
        let err = list_zip(&fixture("kenney.zip"), Some("Nope")).unwrap_err();
        assert_eq!(err.to_string(), "the zip holds no file under Nope");
        let err = list_unitypackage(&synthetic, Some("Nope")).unwrap_err();
        assert_eq!(err.to_string(), "the package holds no file under Nope");
    }

    #[test]
    fn the_tree_counts_files_per_folder_and_one_glb_per_model_stem() {
        let tree = Tree::of(
            [
                "Readme.txt",
                "Models/Crate.fbx",
                "Models/Crate.OBJ",
                "Models/Crate.fbx.meta",
                "Models/Barrel.FBX",
                "Models/Props/Rock.obj",
                "Shipped/Tree.fbx",
                "Shipped/Tree.glb",
                "Textures/.fbx",
            ],
            true,
        );
        let folders: Vec<_> = tree
            .folders
            .iter()
            .map(|f| (f.path.as_str(), f.files, f.total))
            .collect();
        assert_eq!(
            folders,
            [
                ("", 1, 9),
                ("Models", 4, 5),
                ("Models/Props", 1, 1),
                ("Shipped", 2, 2),
                ("Textures", 1, 1),
            ]
        );
        assert_eq!((tree.files, tree.glbs), (9, 3));
        assert_eq!(Tree::of(["Models/Crate.fbx"], false).glbs, 0);
    }

    #[test]
    fn pipelines_are_read_from_the_words_of_a_name() {
        for (name, expected) in [
            ("Crypt_URP", Some("urp")),
            ("URP", Some("urp")),
            ("Universal Render Pipeline", Some("urp")),
            ("Built-In", Some("built-in")),
            ("Built_In_Crypt", Some("built-in")),
            ("BuiltIn", Some("built-in")),
            ("BiRP", Some("built-in")),
            ("Standard", Some("built-in")),
            ("Crypt HDRP", Some("hdrp")),
            ("Crypt", None),
            ("Turpentine", None),
            ("Builtins", None),
        ] {
            assert_eq!(pipeline(name), expected, "{name}");
        }
    }

    #[test]
    fn only_unity_and_fab_targets_follow_the_publisher() {
        let follows = |name: &str, kind: &str| {
            detect(&fixture(name), kind, "Pub")
                .unwrap()
                .into_iter()
                .flat_map(|o| {
                    o.candidates
                        .into_iter()
                        .map(move |c| (o.layout, c.target_follows_publisher, c.target))
                })
                .collect::<Vec<_>>()
        };
        let unity = follows("synthetic.unitypackage", "unitypackage");
        assert_eq!(unity, [("unity", true, "Pub/synthetic".to_string())]);
        let fab = follows("fab-unity.zip", "zip");
        assert_eq!(fab[0], ("fab", true, "Pub/fab-unity".to_string()));
        assert_eq!(fab[1], ("zip", false, "fab-unity".to_string()));
        let kenney = follows("kenney.zip", "zip");
        assert!(kenney.iter().all(|(_, f, _)| !f), "{kenney:?}");
        assert!(kenney
            .iter()
            .any(|(l, _, t)| *l == "kenney" && t == "3D assets/Fixture Car Kit"));
    }
}
