//! The place phase (plan pack-import, Steps): `flatten`, `hoist`, `rename` and `exclude` run over
//! an extracted tree in recipe order, ported from `flatten_pack.py`.
//!
//! `rename` and `exclude` name a file by its source path, so they find it wherever `flatten` and
//! `hoist` put it, and every placed file keeps its source path. Each step moves or drops a file's
//! `.meta` with it. A step that would overwrite a file, or a `rename` or `exclude` whose source the
//! extract no longer produces, fails the build naming the step and the path, before the step
//! moves anything.

use std::fmt;
use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::unitypackage::{below, clean, join, ExtractedFile};
use crate::structure::is_package_file;

/// Files that don't keep the pack root's only folder from being a wrapper: docs and `.meta`
/// files, as `flatten_pack.py`'s `LOOSE`, and `datapackage.json`.
const LOOSE: [&str; 6] = ["txt", "md", "pdf", "rtf", "meta", "url"];

/// A place step as `import_steps` stores it: `kind` in its column, the other fields as `options`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlaceStep {
    /// Merges the root's only folder into the root while nothing else there is an asset,
    /// repeatedly.
    Flatten,
    /// Moves one folder's contents up to the root. A pack without the folder is left as it is.
    Hoist { folder: String },
    /// Renames the file with this source path to `name`, in the folder it is in.
    Rename { source: Vec<String>, name: String },
    /// Drops the file with this source path.
    Exclude { source: Vec<String> },
}

impl fmt::Display for PlaceStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Flatten => write!(f, "flatten"),
            Self::Hoist { folder } => write!(f, "hoist {folder}"),
            Self::Rename { source, name } => write!(f, "rename {} to {name}", show(source)),
            Self::Exclude { source } => write!(f, "exclude {}", show(source)),
        }
    }
}

/// Runs `steps` in order over the tree at `root`, whose files `files` lists, and keeps `files`
/// in step with it, sorted by path.
pub fn place(root: &Path, files: &mut Vec<ExtractedFile>, steps: &[PlaceStep]) -> Result<()> {
    place_in(Tree::Disk(root), files, steps)
}

/// Runs `steps` over `files` alone, as `place` would over a tree holding exactly those files,
/// moving nothing on disk: an import's scan previews a build with it (`super::scan`).
pub fn place_listed(files: &mut Vec<ExtractedFile>, steps: &[PlaceStep]) -> Result<()> {
    place_in(Tree::Listed, files, steps)
}

fn place_in(tree: Tree, files: &mut Vec<ExtractedFile>, steps: &[PlaceStep]) -> Result<()> {
    for step in steps {
        let result = match step {
            PlaceStep::Flatten => flatten(tree, files),
            PlaceStep::Hoist { folder } => hoist(tree, files, folder),
            PlaceStep::Rename { source, name } => rename(tree, files, source, name),
            PlaceStep::Exclude { source } => exclude(tree, files, source),
        };
        result.map_err(|err| anyhow!("{step}: {err:#}"))?;
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(())
}

/// The tree a place step works on: on disk below a folder, or only the file list (`Listed`),
/// whose folders are those holding a file. Its moves are no-ops: the steps keep the list.
#[derive(Clone, Copy)]
enum Tree<'a> {
    Disk(&'a Path),
    Listed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Folder,
    /// A link or another special file, on disk.
    Other,
}

impl Tree<'_> {
    /// The names directly in `folder` (`""` for the root), with their kinds.
    fn entries(self, files: &[ExtractedFile], folder: &str) -> Result<Vec<(String, Kind)>> {
        let root = match self {
            Tree::Disk(root) => root,
            Tree::Listed => {
                let mut names = std::collections::BTreeMap::new();
                for file in files {
                    let rest = if folder.is_empty() {
                        Some(file.path.as_str())
                    } else {
                        below(&file.path, folder).filter(|r| !r.is_empty())
                    };
                    match rest.map(|rest| rest.split_once('/')) {
                        Some(Some((dir, _))) => names.insert(dir.to_string(), Kind::Folder),
                        Some(None) => names.insert(rest.unwrap().to_string(), Kind::File),
                        None => None,
                    };
                }
                return Ok(names.into_iter().collect());
            }
        };
        let mut entries = Vec::new();
        for entry in fs::read_dir(root.join(folder))? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let kind = if kind.is_dir() {
                Kind::Folder
            } else if kind.is_file() {
                Kind::File
            } else {
                Kind::Other
            };
            entries.push((entry.file_name().to_string_lossy().into_owned(), kind));
        }
        Ok(entries)
    }

    /// What is at `path`, if anything.
    fn kind(self, files: &[ExtractedFile], path: &str) -> Option<Kind> {
        match self {
            Tree::Disk(root) => {
                let meta = root.join(path).symlink_metadata().ok()?;
                Some(if meta.is_dir() {
                    Kind::Folder
                } else if meta.is_file() {
                    Kind::File
                } else {
                    Kind::Other
                })
            }
            Tree::Listed => files.iter().find_map(|f| match below(&f.path, path)? {
                "" => Some(Kind::File),
                _ => Some(Kind::Folder),
            }),
        }
    }

    fn rename(self, from: &str, to: &str) -> std::io::Result<()> {
        match self {
            Tree::Disk(root) => fs::rename(root.join(from), root.join(to)),
            Tree::Listed => Ok(()),
        }
    }

    fn remove_file(self, path: &str) -> std::io::Result<()> {
        match self {
            Tree::Disk(root) => fs::remove_file(root.join(path)),
            Tree::Listed => Ok(()),
        }
    }

    fn remove_dir(self, path: &str) -> std::io::Result<()> {
        match self {
            Tree::Disk(root) => fs::remove_dir(root.join(path)),
            Tree::Listed => Ok(()),
        }
    }
}

fn flatten(tree: Tree, files: &mut Vec<ExtractedFile>) -> Result<()> {
    while let Some(folder) = wrapper(tree, files)? {
        merge(tree, files, &folder)?;
    }
    Ok(())
}

/// The root's only folder, when nothing else there is an asset. Hidden entries don't count.
fn wrapper(tree: Tree, files: &[ExtractedFile]) -> Result<Option<String>> {
    let mut folders = Vec::new();
    for (name, kind) in tree.entries(files, "")? {
        if name.starts_with('.') {
            continue;
        }
        if kind == Kind::Folder {
            folders.push(name);
        } else if kind == Kind::File && !is_loose(&name) {
            return Ok(None);
        }
    }
    Ok(match <[String; 1]>::try_from(folders) {
        Ok([folder]) => Some(folder),
        Err(_) => None,
    })
}

fn is_loose(name: &str) -> bool {
    let ext = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => "",
    };
    is_package_file(name) || LOOSE.iter().any(|l| ext.eq_ignore_ascii_case(l))
}

fn hoist(tree: Tree, files: &mut Vec<ExtractedFile>, folder: &str) -> Result<()> {
    let folder = match clean(folder) {
        Ok(parts) if !parts.is_empty() => parts.join("/"),
        _ => bail!("invalid folder {folder:?}"),
    };
    match tree.kind(files, &folder) {
        Some(Kind::Folder) => merge(tree, files, &folder),
        _ => Ok(()),
    }
}

/// Moves the contents of `folder` into the root, then removes it and its own `.meta`. It checks
/// every name first and moves nothing when one would land on an existing file or folder.
fn merge(tree: Tree, files: &mut Vec<ExtractedFile>, folder: &str) -> Result<()> {
    let own_meta = format!("{folder}.meta");
    let mut kids: Vec<String> = tree
        .entries(files, folder)?
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    kids.sort();
    // A kid named like the folder itself lands where the folder was, once it has moved aside.
    let mut self_named = false;
    let mut clashes = Vec::new();
    for kid in &kids {
        if *kid == folder {
            self_named = true;
        } else if *kid != own_meta && tree.kind(files, kid).is_some() {
            clashes.push(kid.clone());
        }
    }
    if !clashes.is_empty() {
        bail!(
            "merging {folder} into the pack root would overwrite {}",
            clashes.join(", ")
        );
    }

    if tree.kind(files, &own_meta) == Some(Kind::File) {
        tree.remove_file(&own_meta)?;
        files.retain(|f| f.path != own_meta);
    }
    let mut src = folder.to_string();
    if self_named {
        src = (0..)
            .map(|n| format!(".opendam-merge-{n}"))
            .find(|p| tree.kind(files, p).is_none())
            .unwrap();
        tree.rename(folder, &src)
            .with_context(|| format!("moving {folder} aside"))?;
    }
    for kid in &kids {
        tree.rename(&join(&src, kid), kid)
            .with_context(|| format!("moving {kid}"))?;
    }
    tree.remove_dir(&src)?;
    for file in files.iter_mut() {
        if let Some(rest) = below(&file.path, folder).filter(|r| !r.is_empty()) {
            file.path = rest.to_string();
        }
    }
    remove_empty_parents(tree, parent(folder))
}

fn rename(tree: Tree, files: &mut [ExtractedFile], source: &[String], name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        bail!("invalid file name {name:?}");
    }
    let file = find(files, source)?;
    let old = files[file].path.clone();
    let new = join(parent(&old), name);
    if new == old {
        return Ok(());
    }
    let mut moves = vec![(file, old, new.clone())];
    if let Ok(meta) = find(files, &meta_source(source)) {
        moves.push((meta, files[meta].path.clone(), format!("{new}.meta")));
    }
    for (_, _, new) in &moves {
        if tree.kind(files, new).is_some() {
            bail!("{new} already exists");
        }
    }
    for (i, old, new) in moves {
        tree.rename(&old, &new)
            .with_context(|| format!("moving {old}"))?;
        files[i].path = new;
    }
    Ok(())
}

fn exclude(tree: Tree, files: &mut Vec<ExtractedFile>, source: &[String]) -> Result<()> {
    let file = find(files, source)?;
    let meta = find(files, &meta_source(source)).ok();
    let gone: Vec<String> = [Some(file), meta]
        .into_iter()
        .flatten()
        .map(|i| files[i].path.clone())
        .collect();
    for path in &gone {
        tree.remove_file(path)
            .with_context(|| format!("removing {path}"))?;
    }
    files.retain(|f| !gone.contains(&f.path));
    remove_empty_parents(tree, parent(&gone[0]))
}

/// The index of the file with this source path.
fn find(files: &[ExtractedFile], source: &[String]) -> Result<usize> {
    files
        .iter()
        .position(|f| f.source_path == source)
        .ok_or_else(|| anyhow!("the extract no longer produces {}", show(source)))
}

/// The source path of a file's `.meta`: its own with `.meta` appended to the last element.
fn meta_source(source: &[String]) -> Vec<String> {
    let mut meta = source.to_vec();
    if let Some(last) = meta.last_mut() {
        last.push_str(".meta");
    }
    meta
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Removes `folder` and each folder above it, below the root, while it is empty. A listed tree
/// has no empty folder.
fn remove_empty_parents(tree: Tree, mut folder: &str) -> Result<()> {
    let Tree::Disk(root) = tree else {
        return Ok(());
    };
    while !folder.is_empty() {
        let path = root.join(folder);
        if !fs::read_dir(&path).is_ok_and(|mut entries| entries.next().is_none()) {
            break;
        }
        fs::remove_dir(&path)?;
        folder = parent(folder);
    }
    Ok(())
}

/// A source path as the manifest stores it, a JSON array.
fn show(source: &[String]) -> String {
    serde_json::to_string(source).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::unitypackage::extract_unitypackage;
    use crate::import::unitypackage::tests::{fixture, golden, on_disk};
    use std::collections::HashMap;

    const ROCK: &str = "Assets/Pack/Built-In/Built-In_Pack/Pack/Art/T_Rock.png";
    const HOUSES: &str = "Assets/Pack/Built-In/Built-In_Pack/Pack/Houses.unitypackage";
    const README: &str = "Assets/Pack/Readme.txt";

    fn source(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    /// `synthetic.unitypackage` extracted into a fresh folder.
    fn synthetic() -> (tempfile::TempDir, Vec<ExtractedFile>) {
        let tmp = tempfile::tempdir().unwrap();
        let extraction =
            extract_unitypackage(&fixture("synthetic.unitypackage"), tmp.path(), None).unwrap();
        (tmp, extraction.files)
    }

    fn paths(files: &[ExtractedFile]) -> Vec<String> {
        files.iter().map(|f| f.path.clone()).collect()
    }

    fn path_of<'f>(files: &'f [ExtractedFile], source: &[String]) -> &'f str {
        &files.iter().find(|f| f.source_path == source).unwrap().path
    }

    fn flatten_and_hoist_art() -> Vec<PlaceStep> {
        vec![
            PlaceStep::Flatten,
            PlaceStep::Hoist {
                folder: "Art".into(),
            },
        ]
    }

    /// Every folder below `root` that holds nothing.
    fn empty_folders(root: &Path) -> Vec<String> {
        walkdir::WalkDir::new(root)
            .min_depth(1)
            .into_iter()
            .map(Result::unwrap)
            .filter(|e| e.file_type().is_dir() && fs::read_dir(e.path()).unwrap().next().is_none())
            .map(|e| e.path().display().to_string())
            .collect()
    }

    /// A tree of `files` (paths relative to the root), each holding its own path.
    fn tree(files: &[&str]) -> (tempfile::TempDir, Vec<ExtractedFile>) {
        let tmp = tempfile::tempdir().unwrap();
        let mut listed = Vec::new();
        for path in files {
            let to = tmp.path().join(path);
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::write(&to, path).unwrap();
            listed.push(ExtractedFile {
                path: path.to_string(),
                source_path: source(&[path]),
                size: path.len() as u64,
            });
        }
        (tmp, listed)
    }

    #[test]
    fn flatten_and_hoist_give_the_golden() {
        let (tmp, mut files) = synthetic();
        let before: HashMap<Vec<String>, u64> = files
            .iter()
            .map(|f| (f.source_path.clone(), f.size))
            .collect();
        place(tmp.path(), &mut files, &flatten_and_hoist_art()).unwrap();

        let expected = golden("synthetic.place.txt");
        assert_eq!(paths(&files), expected);
        assert_eq!(on_disk(tmp.path()), expected);
        assert_eq!(empty_folders(tmp.path()), Vec::<String>::new());

        // Every placed file keeps its source path, and the bytes found at its new path.
        assert_eq!(files.len(), before.len());
        for file in &files {
            assert_eq!(
                before.get(&file.source_path),
                Some(&file.size),
                "{}",
                file.path
            );
            let on_disk = fs::metadata(tmp.path().join(&file.path)).unwrap().len();
            assert_eq!(on_disk, file.size, "{}", file.path);
        }
        assert_eq!(path_of(&files, &source(&[ROCK])), "T_Rock.png");
        assert_eq!(
            path_of(&files, &source(&[HOUSES, ROCK])),
            "Houses/Built-In/Built-In_Pack/Pack/Art/T_Rock.png"
        );
    }

    #[test]
    fn flatten_ignores_docs_metas_and_datapackage() {
        let (tmp, mut files) = tree(&[
            "Readme.md",
            "Readme.md.meta",
            "Notes.PDF",
            "DataPackage.json",
            ".hidden.fbx",
            "Wrap.meta",
            "Wrap/Inner/x.fbx",
            "Wrap/Inner/x.fbx.meta",
        ]);
        place(tmp.path(), &mut files, &[PlaceStep::Flatten]).unwrap();
        // `Wrap/` and `Inner/` both merge, and `Wrap`'s own `.meta` goes with it.
        let expected = [
            ".hidden.fbx",
            "DataPackage.json",
            "Notes.PDF",
            "Readme.md",
            "Readme.md.meta",
            "x.fbx",
            "x.fbx.meta",
        ];
        assert_eq!(paths(&files), expected);
        assert_eq!(on_disk(tmp.path()), expected);

        // An asset beside the folder, or a second folder, keeps it.
        for layout in [["Wrap/x.fbx", "rock.fbx"], ["Docs/a.txt", "Wrap/x.fbx"]] {
            let (tmp, mut files) = tree(&layout);
            place(tmp.path(), &mut files, &[PlaceStep::Flatten]).unwrap();
            assert_eq!(paths(&files), layout);
            assert_eq!(on_disk(tmp.path()), layout);
        }
    }

    #[test]
    fn a_wrapper_holding_a_folder_of_its_own_name_merges() {
        let (tmp, mut files) = tree(&["Pack.meta", "Pack/Pack.meta", "Pack/Pack/x.fbx"]);
        place(tmp.path(), &mut files, &[PlaceStep::Flatten]).unwrap();
        assert_eq!(paths(&files), ["x.fbx"]);
        assert_eq!(on_disk(tmp.path()), ["x.fbx"]);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_step_that_would_overwrite_a_file_fails_naming_it() {
        // Flatten, before it moves anything.
        let layout = ["Readme.txt", "Wrap/Readme.txt", "Wrap/x.fbx"];
        let (tmp, mut files) = tree(&layout);
        let err = place(tmp.path(), &mut files, &[PlaceStep::Flatten]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "flatten: merging Wrap into the pack root would overwrite Readme.txt"
        );
        assert_eq!(on_disk(tmp.path()), layout);

        // Hoist, onto a file an earlier rename put at the root, `.meta` and all.
        let (tmp, mut files) = synthetic();
        let mut steps = vec![
            PlaceStep::Flatten,
            PlaceStep::Rename {
                source: source(&[README]),
                name: "T_Rock.png".into(),
            },
        ];
        place(tmp.path(), &mut files, &steps).unwrap();
        let placed = on_disk(tmp.path());
        steps.push(PlaceStep::Hoist {
            folder: "Art".into(),
        });
        let err = place(tmp.path(), &mut files, &steps[2..]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "hoist Art: merging Art into the pack root would overwrite T_Rock.png, T_Rock.png.meta"
        );
        assert_eq!(on_disk(tmp.path()), placed);

        // Rename, onto a file or onto another file's `.meta`.
        let (tmp, mut files) = synthetic();
        place(tmp.path(), &mut files, &flatten_and_hoist_art()).unwrap();
        for name in ["Readme.txt", "Readme.txt.meta"] {
            let step = PlaceStep::Rename {
                source: source(&[ROCK]),
                name: name.into(),
            };
            let err = place(tmp.path(), &mut files, &[step]).unwrap_err();
            assert!(err
                .to_string()
                .starts_with(&format!("rename [\"{ROCK}\"] to {name}: {name}")));
            assert!(err.to_string().ends_with(" already exists"), "{err}");
        }
        assert_eq!(on_disk(tmp.path()), golden("synthetic.place.txt"));
    }

    #[test]
    fn a_source_the_extract_no_longer_produces_fails_naming_it() {
        let gone = source(&["Assets/Pack/Gone.png"]);
        let houses = source(&[HOUSES]);
        for (step, named) in [
            (
                PlaceStep::Rename {
                    source: gone.clone(),
                    name: "New.png".into(),
                },
                &gone,
            ),
            (
                PlaceStep::Exclude {
                    source: gone.clone(),
                },
                &gone,
            ),
            // The nested package's path alone names no file.
            (
                PlaceStep::Exclude {
                    source: houses.clone(),
                },
                &houses,
            ),
        ] {
            let (tmp, mut files) = synthetic();
            let err = place(tmp.path(), &mut files, std::slice::from_ref(&step)).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("{step}: the extract no longer produces {}", show(named))
            );
            assert_eq!(on_disk(tmp.path()), golden("synthetic.extract.txt"));
        }
    }

    #[test]
    fn rename_by_a_nested_source_path_renames_only_that_file() {
        let (tmp, mut files) = synthetic();
        let mut steps = flatten_and_hoist_art();
        steps.push(PlaceStep::Rename {
            source: source(&[HOUSES, ROCK]),
            name: "T_Stone.png".into(),
        });
        place(tmp.path(), &mut files, &steps).unwrap();

        let art = "Houses/Built-In/Built-In_Pack/Pack/Art";
        let expected: Vec<String> = golden("synthetic.place.txt")
            .into_iter()
            .map(|p| p.replace(&format!("{art}/T_Rock.png"), &format!("{art}/T_Stone.png")))
            .collect();
        let mut sorted = expected.clone();
        sorted.sort();
        assert_eq!(paths(&files), sorted);
        assert_eq!(on_disk(tmp.path()), sorted);
        assert_eq!(
            path_of(&files, &source(&[HOUSES, ROCK])),
            format!("{art}/T_Stone.png")
        );
        assert_eq!(
            path_of(&files, &source(&[HOUSES, &format!("{ROCK}.meta")])),
            format!("{art}/T_Stone.png.meta")
        );
        assert_eq!(path_of(&files, &source(&[ROCK])), "T_Rock.png");
        assert_eq!(
            path_of(&files, &source(&[&format!("{ROCK}.meta")])),
            "T_Rock.png.meta"
        );
    }

    #[test]
    fn steps_find_files_wherever_earlier_steps_put_them() {
        // The same rename before or after the flatten and hoist ends in the same place.
        let rename = PlaceStep::Rename {
            source: source(&[ROCK]),
            name: "T_Boulder.png".into(),
        };
        let (before, mut before_files) = synthetic();
        let mut steps = vec![rename.clone()];
        steps.extend(flatten_and_hoist_art());
        place(before.path(), &mut before_files, &steps).unwrap();
        let (after, mut after_files) = synthetic();
        let mut steps = flatten_and_hoist_art();
        steps.push(rename);
        place(after.path(), &mut after_files, &steps).unwrap();
        assert_eq!(before_files, after_files);
        assert!(on_disk(after.path()).contains(&"T_Boulder.png.meta".to_string()));
    }

    #[test]
    fn exclude_drops_the_file_and_its_meta() {
        let (tmp, mut files) = synthetic();
        let log = "Assets/Pack/Built-In/Built-In_Pack/Pack/Art/Models/SM_Log.fbx";
        let mut steps = flatten_and_hoist_art();
        steps.push(PlaceStep::Exclude {
            source: source(&[log]),
        });
        steps.push(PlaceStep::Exclude {
            source: source(&[README]),
        });
        place(tmp.path(), &mut files, &steps).unwrap();

        let expected: Vec<String> = golden("synthetic.place.txt")
            .into_iter()
            .filter(|p| !p.starts_with("Models/SM_Log.fbx") && !p.starts_with("Readme.txt"))
            .collect();
        assert_eq!(paths(&files), expected);
        assert_eq!(on_disk(tmp.path()), expected);
        // `Models/` held nothing else, so it goes too.
        assert_eq!(empty_folders(tmp.path()), Vec::<String>::new());
    }

    #[test]
    fn hoist_of_a_missing_folder_changes_nothing() {
        let (tmp, mut files) = synthetic();
        let steps = [PlaceStep::Hoist {
            folder: "Meshes".into(),
        }];
        place(tmp.path(), &mut files, &steps).unwrap();
        assert_eq!(on_disk(tmp.path()), golden("synthetic.extract.txt"));

        let steps = [PlaceStep::Hoist {
            folder: "../x".into(),
        }];
        let err = place(tmp.path(), &mut files, &steps).unwrap_err();
        assert!(err.to_string().contains("invalid folder"), "{err}");
    }

    #[test]
    fn rename_fails_when_only_the_meta_would_overwrite_a_file() {
        let layout = ["a.png", "a.png.meta", "b.png.meta"];
        let (tmp, mut files) = tree(&layout);
        let step = PlaceStep::Rename {
            source: source(&["a.png"]),
            name: "b.png".into(),
        };
        let err = place(tmp.path(), &mut files, &[step]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "rename [\"a.png\"] to b.png: b.png.meta already exists"
        );
        assert_eq!(paths(&files), layout);
        assert_eq!(on_disk(tmp.path()), layout);
        assert_eq!(
            fs::read(tmp.path().join("b.png.meta")).unwrap(),
            b"b.png.meta"
        );
    }

    #[test]
    fn rename_refuses_a_name_that_is_not_a_plain_file_name() {
        let layout = ["Art/a.png", "Art/a.png.meta"];
        for name in ["", ".", "..", "x/y.png", "../a.png"] {
            let (tmp, mut files) = tree(&layout);
            let step = PlaceStep::Rename {
                source: source(&["Art/a.png"]),
                name: name.into(),
            };
            let err = place(tmp.path(), &mut files, &[step]).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("rename [\"Art/a.png\"] to {name}: invalid file name {name:?}")
            );
            assert_eq!(paths(&files), layout);
            assert_eq!(on_disk(tmp.path()), layout);
        }
    }

    #[test]
    fn a_listed_tree_places_exactly_as_the_disk() {
        let rename = |src: &str, name: &str| PlaceStep::Rename {
            source: source(&[src]),
            name: name.into(),
        };
        let hoist = |folder: &str| PlaceStep::Hoist {
            folder: folder.into(),
        };
        let exclude = |src: &str| PlaceStep::Exclude {
            source: source(&[src]),
        };
        let synthetic_cases = vec![
            flatten_and_hoist_art(),
            vec![hoist("Meshes"), PlaceStep::Flatten],
            vec![
                rename(ROCK, "T_Boulder.png"),
                PlaceStep::Flatten,
                hoist("Art"),
            ],
            vec![
                PlaceStep::Flatten,
                rename(README, "T_Rock.png"),
                hoist("Art"),
            ],
            vec![exclude(README), PlaceStep::Flatten, exclude(ROCK)],
            vec![exclude("Assets/Pack/Gone.png")],
            vec![PlaceStep::Flatten, hoist("../x")],
        ];
        let mut cases: Vec<(Option<&[&str]>, Vec<PlaceStep>)> =
            synthetic_cases.into_iter().map(|s| (None, s)).collect();
        let layouts: [&[&str]; 5] = [
            &["Readme.txt", "Wrap/Readme.txt", "Wrap/x.fbx"],
            &["Pack.meta", "Pack/Pack.meta", "Pack/Pack/x.fbx"],
            &[
                "Readme.md",
                "DataPackage.json",
                ".hidden.fbx",
                "Wrap.meta",
                "Wrap/Inner/x.fbx",
            ],
            &["Docs/a.txt", "Wrap/x.fbx"],
            &["a.png", "a.png.meta", "b.png.meta"],
        ];
        for layout in layouts {
            cases.push((Some(layout), vec![PlaceStep::Flatten]));
            cases.push((Some(layout), vec![rename("a.png", "b.png")]));
        }

        for (layout, steps) in cases {
            let (tmp, mut on_disk) = match layout {
                Some(layout) => tree(layout),
                None => synthetic(),
            };
            let mut listed = on_disk.clone();
            let disk = place(tmp.path(), &mut on_disk, &steps).map_err(|e| e.to_string());
            let list = place_listed(&mut listed, &steps).map_err(|e| e.to_string());
            assert_eq!(list, disk, "{layout:?} {steps:?}");
            if disk.is_ok() {
                assert_eq!(listed, on_disk, "{layout:?} {steps:?}");
            }
        }
    }
}
