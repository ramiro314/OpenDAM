//! Outside changes (plan pack-import, Edits after an import, R39, R42). Before a Rebuild, the
//! live pack folder is compared with its current manifest, by size and mtime, hashing only on a
//! mismatch. Each difference offers only the choices its file's origin can honour:
//!
//! | Difference | Extracted file | Generated GLB | `datapackage.json` | Kept file |
//! |---|---|---|---|---|
//! | Changed | keep yours, use the recipe's | use the recipe's | keep yours (turns the metadata step off), use the recipe's | keep yours |
//! | Missing | record as `exclude`, restore | restore | turn the metadata step off, restore | forget |
//!
//! An added file is kept or discarded, or only discarded at a path the build writes itself (a
//! GLB beside a model while `unity_glb` is on, `datapackage.json` while the metadata step is on).
//!
//! The choices travel with the run (`import_runs.drift`), each with the difference it was made
//! for, so a build checks, before it stages and again under the lock, that the folder still
//! differs exactly so. A kept file is carried into the new build as `kept`: one that replaces a
//! recipe file keeps that file's source path, so the steps place it, and the carry moves its
//! row, as the file's; one added outside OpenDAM has none and keeps its path.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sqlx::{Pool, Sqlite};

use super::build::file_stamp;
use super::records::{self, DriftEdits, ManifestRow, Origin, Recipe};
use crate::structure::is_package_file;

/// What the folder holds against the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Change {
    /// A file the manifest doesn't hold.
    Added,
    /// A manifest file with other bytes.
    Changed,
    /// A manifest file that is gone.
    Missing,
    /// A kept file the recipe now writes over (a GLB, `datapackage.json`) or leaves out (an
    /// `exclude` of its source path), so it can't be carried as it is.
    Clash,
}

/// What made the file a difference is about, as the choices depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// From an entry of the original.
    Extracted,
    /// A `unity_glb` GLB.
    Generated,
    /// The metadata step's `datapackage.json`.
    Package,
    /// Kept from outside changes at an earlier Rebuild.
    Kept,
    /// Added outside OpenDAM.
    Outside,
}

/// What to do about one difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Choice {
    /// Keep yours: carry the folder's file into the new build as `kept`.
    Keep,
    /// Use the recipe's: the build writes its own file there.
    Recipe,
    /// Record the missing file as an `exclude` step.
    Exclude,
    /// Restore the missing file from the recipe.
    Restore,
    /// Turn the metadata step off, as an in-app delete of `datapackage.json` does.
    MetadataOff,
    /// Drop a missing kept file from the kept files.
    Forget,
    /// Discard an added file: it goes with the previous build's folder.
    Discard,
}

/// One difference between the pack folder and its manifest, with the choices it offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Difference {
    /// Relative to the library root.
    pub path: String,
    pub change: Change,
    pub kind: Kind,
    /// The folder's file now: its size and sha256, `None` for a missing file.
    pub size: Option<i64>,
    pub sha256: Option<String>,
    /// The choices its kind can honour, the default first.
    pub choices: Vec<Choice>,
    /// Why it offers only one choice, when it does.
    pub reason: Option<&'static str>,
    /// The manifest file's source path (its JSON array), when it has one.
    #[serde(skip)]
    pub source_path: Option<String>,
}

/// A choice as a Rebuild sends it, and its run carries it: the difference it was made for, by
/// path, change and the bytes seen, and the choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chosen {
    pub path: String,
    pub change: Change,
    #[serde(default)]
    pub sha256: Option<String>,
    pub choice: Choice,
}

/// What a Rebuild of the folder sees when the choices don't match it.
pub const STALE: &str = "The pack changed outside OpenDAM since it was checked: check it again";

/// A file the next build carries from the pack folder, kept from outside changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptFile {
    /// Where it is now, relative to the library root.
    pub path: String,
    /// The recipe file it replaces, by source path, or `None` for one added outside OpenDAM.
    pub source_path: Option<Vec<String>>,
    /// Its bytes, as the drift check (or the manifest, unchanged) saw them.
    pub sha256: String,
}

/// The paths a build writes itself, which an added file can't keep.
struct Writes<'a> {
    target: &'a str,
    metadata: bool,
    convert: bool,
    /// The manifest's models by folder and stem, lowercased, for the GLBs `unity_glb` writes.
    models: HashSet<(String, String)>,
    /// The source paths (JSON arrays) the recipe's enabled `exclude` steps leave out.
    excluded: HashSet<String>,
}

impl<'a> Writes<'a> {
    fn of(recipe: &'a Recipe, manifest: &[ManifestRow]) -> Self {
        let on = |kind: &str| recipe.steps.iter().any(|(k, _)| k == kind);
        let models = manifest
            .iter()
            .filter(|row| !row.origin.generated())
            .filter_map(|row| {
                let (folder, stem, ext) = split(&row.path);
                ["fbx", "obj"]
                    .contains(&ext.as_str())
                    .then(|| (folder.to_string(), stem))
            })
            .collect();
        let excluded = recipe
            .steps
            .iter()
            .filter(|(kind, _)| kind == "exclude")
            .filter_map(|(_, options)| {
                let source: Vec<String> = serde_json::from_value(options["source"].clone()).ok()?;
                Some(records::source_json(&source))
            })
            .collect();
        Writes {
            target: &recipe.target,
            metadata: on("metadata"),
            convert: on("unity_glb"),
            models,
            excluded,
        }
    }

    /// Why the kept file `row` can't be carried as it is under the recipe now, if it can't.
    fn clash(&self, row: &ManifestRow) -> Option<&'static str> {
        if self.writes(&row.path) {
            return Some(
                "The build now writes its own file at this path: discard yours, or turn off \
                 the step that writes it to keep yours",
            );
        }
        row.source_path
            .as_ref()
            .filter(|source| self.excluded.contains(*source))
            .map(|_| "An exclude step now leaves this file out, so the kept copy goes too")
    }

    fn writes(&self, path: &str) -> bool {
        if self.metadata && is_root_package(self.target, path) {
            return true;
        }
        let (folder, stem, ext) = split(path);
        self.convert && ext == "glb" && self.models.contains(&(folder.to_string(), stem))
    }
}

/// A path's folder, then its stem and extension, lowercased.
fn split(path: &str) -> (&str, String, String) {
    let (folder, name) = path.rsplit_once('/').unwrap_or(("", path));
    let name = name.to_ascii_lowercase();
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (folder, stem.to_string(), ext.to_string()),
        _ => (folder, name, String::new()),
    }
}

/// Whether `path` is a `datapackage.json` (any ASCII case) right in the pack folder `target`.
fn is_root_package(target: &str, path: &str) -> bool {
    path.strip_prefix(target)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| !name.contains('/') && is_package_file(name))
}

/// Pack `pack_id`'s differences now, in the library at `root`. A pack that was never built has
/// none: its first build needs its folder not to exist.
pub async fn check(
    pool: &Pool<Sqlite>,
    root: &Path,
    pack_id: &str,
) -> Result<Vec<Difference>, String> {
    let recipe = records::recipe(pool, pack_id)
        .await
        .map_err(|e| format!("Database error: {e}"))?;
    let Some(build) = &recipe.current_build else {
        return Ok(Vec::new());
    };
    let manifest = records::manifest(pool, pack_id, build)
        .await
        .map_err(|e| format!("Database error: {e}"))?;
    scan(root, &recipe, &manifest).await
}

/// The differences between the pack folder of `recipe`, in the library at `root`, and
/// `manifest`, its current build's rows. A folder that is gone, deleted outside OpenDAM, misses
/// every file.
pub async fn scan(
    root: &Path,
    recipe: &Recipe,
    manifest: &[ManifestRow],
) -> Result<Vec<Difference>, String> {
    let (live, recipe, manifest) = (root.join(&recipe.target), recipe.clone(), manifest.to_vec());
    tokio::task::spawn_blocking(move || {
        differences(&live, &recipe, &manifest)
            .map_err(|e| format!("The pack folder {} can't be read: {e}", recipe.target))
    })
    .await
    .map_err(|e| format!("The drift check stopped: {e}"))?
}

fn differences(
    live: &Path,
    recipe: &Recipe,
    manifest: &[ManifestRow],
) -> std::io::Result<Vec<Difference>> {
    let prefix = format!("{}/", recipe.target);
    let on_disk: HashMap<String, ()> = match live.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
        Err(e) => return Err(e),
        Ok(meta) if !meta.is_dir() => {
            return Err(std::io::Error::other("it is no folder"));
        }
        Ok(_) => files(live)?
            .into_iter()
            .map(|rel| (format!("{prefix}{rel}"), ()))
            .collect(),
    };
    let writes = Writes::of(recipe, manifest);
    // With the whole folder gone, deleted outside OpenDAM, restoring is the likely wish.
    let gone = on_disk.is_empty() && !live.exists();
    let mut found = Vec::new();
    for row in manifest {
        let kind = match row.origin {
            Origin::Extracted => Kind::Extracted,
            Origin::Kept => Kind::Kept,
            Origin::Generated if is_root_package(&recipe.target, &row.path) => Kind::Package,
            Origin::Generated => Kind::Generated,
        };
        let path = live.join(&row.path[prefix.len()..]);
        let change = if !on_disk.contains_key(&row.path) {
            Some((Change::Missing, None, None))
        } else {
            let (size, mtime_ns) = file_stamp(&path)?;
            if size as i64 == row.size && mtime_ns == row.mtime_ns {
                None
            } else {
                let sha256 = crate::export::hashes::sha256_file(&path)?;
                (sha256 != row.sha256).then_some((Change::Changed, Some(size as i64), Some(sha256)))
            }
        };
        let clash = (kind == Kind::Kept && on_disk.contains_key(&row.path))
            .then(|| writes.clash(row))
            .flatten();
        if let Some(reason) = clash {
            let (size, sha256) = match change {
                Some((_, size, sha256)) => (size, sha256),
                None => (Some(row.size), Some(row.sha256.clone())),
            };
            found.push(Difference {
                path: row.path.clone(),
                change: Change::Clash,
                kind,
                size,
                sha256,
                choices: vec![Choice::Discard],
                reason: Some(reason),
                source_path: row.source_path.clone(),
            });
            continue;
        }
        if let Some((change, size, sha256)) = change {
            let (mut choices, reason) = offered(change, kind);
            if gone && change == Change::Missing {
                choices.sort_by_key(|c| *c != Choice::Restore);
            }
            found.push(Difference {
                path: row.path.clone(),
                change,
                kind,
                size,
                sha256,
                choices,
                reason,
                source_path: row.source_path.clone(),
            });
        }
    }
    let listed: HashSet<&str> = manifest.iter().map(|r| r.path.as_str()).collect();
    let mut added: Vec<&String> = on_disk
        .keys()
        .filter(|p| !listed.contains(p.as_str()))
        .collect();
    added.sort();
    for path in added {
        let file = live.join(&path[prefix.len()..]);
        let size = file.metadata()?.len() as i64;
        let sha256 = crate::export::hashes::sha256_file(&file)?;
        let (choices, reason) = if writes.writes(path) {
            (vec![Choice::Discard], Some(WRITTEN))
        } else {
            (vec![Choice::Keep, Choice::Discard], None)
        };
        found.push(Difference {
            path: path.clone(),
            change: Change::Added,
            kind: Kind::Outside,
            size: Some(size),
            sha256: Some(sha256),
            choices,
            reason,
            source_path: None,
        });
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(found)
}

const WRITTEN: &str = "The build writes its own file at this path";

/// The choices a `change` to a file of `kind` offers, the default first, and why there is only
/// one, when there is.
fn offered(change: Change, kind: Kind) -> (Vec<Choice>, Option<&'static str>) {
    use Choice::*;
    match (change, kind) {
        (Change::Changed, Kind::Extracted | Kind::Package) => (vec![Keep, Recipe], None),
        (Change::Changed, Kind::Generated) => (
            vec![Recipe],
            Some("The convert step writes this GLB again, so a kept copy would clash with it"),
        ),
        (Change::Changed, Kind::Kept) => (vec![Keep], Some("A kept file keeps its new bytes")),
        (Change::Missing, Kind::Extracted) => (vec![Exclude, Restore], None),
        (Change::Missing, Kind::Generated) => (
            vec![Restore],
            Some(
                "No step drops one GLB: delete its model in the app, or turn the convert step off",
            ),
        ),
        (Change::Missing, Kind::Package) => (vec![MetadataOff, Restore], None),
        (Change::Missing, Kind::Kept) => (
            vec![Forget],
            Some("A kept file isn't from the original, so a rebuild can't recreate it"),
        ),
        (Change::Clash, _) => (vec![Discard], None),
        (_, Kind::Outside) | (Change::Added, _) => (vec![Keep, Discard], None),
    }
}

/// Every regular file below `folder`, relative to it and `/`-separated. Anything else but a
/// folder is an error: a pack folder holds only files and folders.
fn files(folder: &Path) -> std::io::Result<Vec<String>> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(folder).min_depth(1) {
        let entry = entry?;
        let meta = entry.path().symlink_metadata()?;
        if meta.is_dir() {
            continue;
        }
        let rel = entry.path().strip_prefix(folder).expect("below the folder");
        let rel = rel.to_string_lossy().into_owned();
        if !meta.is_file() {
            return Err(std::io::Error::other(format!("{rel} isn't a regular file")));
        }
        files.push(rel);
    }
    Ok(files)
}

/// Checks that `chosen` answers exactly the differences `found`, each with a choice its file
/// offers. Returns the first path that doesn't match, if any.
pub fn unmatched(found: &[Difference], chosen: &[Chosen]) -> Option<String> {
    let mut by_path: HashMap<&str, &Chosen> = HashMap::new();
    for c in chosen {
        if by_path.insert(c.path.as_str(), c).is_some() {
            return Some(c.path.clone());
        }
    }
    for difference in found {
        let ok = by_path.get(difference.path.as_str()).is_some_and(|c| {
            c.change == difference.change
                && c.sha256 == difference.sha256
                && difference.choices.contains(&c.choice)
        });
        if !ok {
            return Some(difference.path.clone());
        }
    }
    let found: HashSet<&str> = found.iter().map(|d| d.path.as_str()).collect();
    chosen
        .iter()
        .find(|c| !found.contains(c.path.as_str()))
        .map(|c| c.path.clone())
}

/// What `chosen`, matched against `found`, asks of `recipe` at the Rebuild, so each choice is
/// honoured whatever an earlier Rebuild that never committed recorded: an `exclude` for each
/// missing extracted file left out, and none (the enabled one turned off) for one restored or
/// rebuilt from the recipe; the metadata step off when a missing `datapackage.json` turns it off
/// or a changed one is kept, and on when it is restored or rebuilt from the recipe.
pub fn edits(recipe: &Recipe, found: &[Difference], chosen: &[Chosen]) -> DriftEdits {
    let writes = Writes::of(recipe, &[]);
    let mut edits = DriftEdits::default();
    for (difference, choice) in pairs(found, chosen) {
        let source = difference.source_path.as_deref();
        match (difference.kind, choice) {
            (Kind::Extracted, Choice::Exclude) => {
                if let Some(source) = source.and_then(|s| serde_json::from_str(s).ok()) {
                    edits.excludes.push(source);
                }
            }
            (Kind::Extracted, Choice::Restore | Choice::Recipe) => {
                if let Some(source) = source.filter(|s| writes.excluded.contains(*s)) {
                    edits
                        .includes
                        .extend(serde_json::from_str::<Vec<String>>(source).ok());
                }
            }
            (Kind::Package, Choice::MetadataOff | Choice::Keep) => edits.metadata_off = true,
            (Kind::Package, Choice::Restore | Choice::Recipe) if !writes.metadata => {
                edits.metadata_on = true
            }
            _ => {}
        }
    }
    edits
}

/// The files the next build carries from the pack folder: the manifest's kept files but those
/// forgotten (with their new bytes when changed), and every changed or added file kept. A kept
/// `datapackage.json` is a file of its own: the metadata step that wrote it is off.
pub fn kept(manifest: &[ManifestRow], found: &[Difference], chosen: &[Chosen]) -> Vec<KeptFile> {
    let answers: HashMap<&str, (&Difference, Choice)> = pairs(found, chosen)
        .map(|(d, c)| (d.path.as_str(), (d, c)))
        .collect();
    let source = |row: Option<&ManifestRow>| {
        row.and_then(|r| r.source_path.as_deref())
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
            .filter(|s| !s.is_empty())
    };
    let mut kept = Vec::new();
    for row in manifest.iter().filter(|r| r.origin == Origin::Kept) {
        let sha256 = match answers.get(row.path.as_str()) {
            None => row.sha256.clone(),
            Some((d, Choice::Keep)) => d.sha256.clone().unwrap_or_default(),
            Some(_) => continue,
        };
        kept.push(KeptFile {
            path: row.path.clone(),
            source_path: source(Some(row)),
            sha256,
        });
    }
    for (difference, choice) in answers.values() {
        if *choice != Choice::Keep || difference.kind == Kind::Kept {
            continue;
        }
        let row = manifest.iter().find(|r| r.path == difference.path);
        kept.push(KeptFile {
            path: difference.path.clone(),
            source_path: match difference.kind {
                Kind::Extracted => source(row),
                _ => None,
            },
            sha256: difference.sha256.clone().unwrap_or_default(),
        });
    }
    kept.sort_by(|a, b| a.path.cmp(&b.path));
    kept
}

fn pairs<'a>(
    found: &'a [Difference],
    chosen: &'a [Chosen],
) -> impl Iterator<Item = (&'a Difference, Choice)> + 'a {
    found.iter().filter_map(move |d| {
        chosen
            .iter()
            .find(|c| c.path == d.path)
            .map(|c| (d, c.choice))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn difference(change: Change, kind: Kind) -> Difference {
        let (choices, reason) = offered(change, kind);
        Difference {
            path: "P/a".into(),
            change,
            kind,
            size: None,
            sha256: None,
            choices,
            reason,
            source_path: Some(r#"["Assets/a"]"#.into()),
        }
    }

    #[test]
    fn each_origin_offers_only_the_choices_it_can_honour() {
        use Choice::*;
        let cases = [
            (Change::Changed, Kind::Extracted, vec![Keep, Recipe]),
            (Change::Changed, Kind::Generated, vec![Recipe]),
            (Change::Changed, Kind::Package, vec![Keep, Recipe]),
            (Change::Changed, Kind::Kept, vec![Keep]),
            (Change::Missing, Kind::Extracted, vec![Exclude, Restore]),
            (Change::Missing, Kind::Generated, vec![Restore]),
            (Change::Missing, Kind::Package, vec![MetadataOff, Restore]),
            (Change::Missing, Kind::Kept, vec![Forget]),
            (Change::Added, Kind::Outside, vec![Keep, Discard]),
        ];
        for (change, kind, want) in cases {
            let (choices, reason) = offered(change, kind);
            assert_eq!(choices, want, "{change:?} {kind:?}");
            assert_eq!(reason.is_some(), choices.len() == 1, "{change:?} {kind:?}");
        }
    }

    #[test]
    fn choices_must_answer_every_difference_with_one_it_offers() {
        let found = [difference(Change::Missing, Kind::Generated)];
        let chosen = |choice| Chosen {
            path: "P/a".into(),
            change: Change::Missing,
            sha256: None,
            choice,
        };
        assert_eq!(unmatched(&found, &[chosen(Choice::Restore)]), None);
        assert_eq!(
            unmatched(&found, &[chosen(Choice::Keep)]),
            Some("P/a".into())
        );
        assert_eq!(unmatched(&found, &[]), Some("P/a".into()));
        assert_eq!(
            unmatched(&[], &[chosen(Choice::Restore)]),
            Some("P/a".into())
        );
        assert_eq!(
            unmatched(&found, &[chosen(Choice::Restore), chosen(Choice::Restore)]),
            Some("P/a".into()),
            "one choice per difference"
        );
        let mut changed = chosen(Choice::Restore);
        changed.sha256 = Some("other".into());
        assert_eq!(unmatched(&found, &[changed]), Some("P/a".into()));
    }

    #[test]
    fn an_added_file_at_a_path_the_build_writes_offers_only_discard() {
        let recipe = Recipe {
            pack_id: "p".into(),
            target: "P".into(),
            recipe_rev: 0,
            current_build: Some("b".into()),
            swap_state: None,
            original_path: String::new(),
            original_size: 0,
            original_mtime_ns: 0,
            original_sha256: String::new(),
            kind: "unitypackage".into(),
            store_page: None,
            steps: vec![
                ("unity_glb".into(), serde_json::json!({})),
                ("metadata".into(), serde_json::json!({})),
            ],
        };
        let row = ManifestRow {
            path: "P/Models/Rock.FBX".into(),
            size: 1,
            mtime_ns: 1,
            sha256: String::new(),
            source_path: Some("[]".into()),
            origin: Origin::Extracted,
        };
        let writes = Writes::of(&recipe, &[row]);
        assert!(writes.writes("P/Models/rock.glb"));
        assert!(writes.writes("P/DataPackage.json"));
        assert!(!writes.writes("P/Models/Tree.glb"));
        assert!(!writes.writes("P/Rock.glb"));
        assert!(!writes.writes("P/Models/datapackage.json"));
    }
}
