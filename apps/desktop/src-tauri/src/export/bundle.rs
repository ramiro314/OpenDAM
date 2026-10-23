//! Writing a domain's bundle (plan game-export C15, D18). "Check" builds the bundle into
//! `<root>/<Root>/.D.staging-<uuid>/` beside the domain's folder `D/`, validates it, and diffs it
//! against the previous `D/manifest.json`. "Export" swaps it in by renaming, only when nothing the
//! Check read has changed since (C14). A failed check or a crash never loses the previous bundle:
//! the next Check recovers from a crash between the swap's two renames.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::db::Database;
use crate::export::check::{self, BundleFile, CheckReport, Inspection, Problem, Status};
use crate::export::fbx_facts;
use crate::export::fingerprint::Fingerprint;
use crate::export::infer;
use crate::export::{contract, hashes};

const MANIFEST: &str = "manifest.json";

/// One asset of a manifest, as the diff names it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffEntry {
    pub kind: String,
    pub asset_name: String,
    /// A texture's role, `None` for a model or an animation
    pub role: Option<String>,
}

/// What an export changes in the bundle (C14): the assets it adds, changes (their file's sha256
/// or any other field of their entry) and removes, by kind, `assetName` and role, so a renamed
/// asset is a removal plus an addition, and the packs whose entry it adds, changes or removes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diff {
    /// The previous bundle's `exportedAt`, `None` when there is no previous bundle
    pub previous_exported_at: Option<String>,
    pub added: Vec<DiffEntry>,
    pub changed: Vec<DiffEntry>,
    pub removed: Vec<DiffEntry>,
    /// The ids of the packs whose manifest entry differs (a licence, name or origin edit, say),
    /// or that only one of the two manifests lists, sorted
    pub packs: Vec<String>,
}

fn entry_of(asset: &Value) -> DiffEntry {
    let text = |key: &str| asset[key].as_str().map(str::to_string);
    DiffEntry {
        kind: text("kind").unwrap_or_default(),
        asset_name: text("assetName").unwrap_or_default(),
        role: text("role"),
    }
}

/// The diff from the manifest `previous` (none for a first export) to `next`, each list in the
/// manifest's order.
pub fn diff(previous: Option<&Value>, next: &Value) -> Diff {
    fn assets(manifest: Option<&Value>) -> &[Value] {
        manifest
            .and_then(|m| m["assets"].as_array())
            .map_or(&[], Vec::as_slice)
    }
    let before: HashMap<DiffEntry, &Value> =
        assets(previous).iter().map(|a| (entry_of(a), a)).collect();
    let after: HashSet<DiffEntry> = assets(Some(next)).iter().map(entry_of).collect();
    let mut diff = Diff {
        previous_exported_at: previous.and_then(|m| m["exportedAt"].as_str().map(str::to_string)),
        ..Diff::default()
    };
    for asset in assets(Some(next)) {
        let entry = entry_of(asset);
        match before.get(&entry) {
            None => diff.added.push(entry),
            Some(old) if *old != asset => diff.changed.push(entry),
            Some(_) => {}
        }
    }
    diff.removed = assets(previous)
        .iter()
        .map(entry_of)
        .filter(|e| !after.contains(e))
        .collect();
    let packs = |m: Option<&Value>| -> HashMap<String, Value> {
        m.and_then(|m| m["packs"].as_array())
            .into_iter()
            .flatten()
            .map(|p| (p["id"].as_str().unwrap_or_default().to_string(), p.clone()))
            .collect()
    };
    let (before, after) = (packs(previous), packs(Some(next)));
    let ids: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    diff.packs = ids
        .into_iter()
        .filter(|id| before.get(*id) != after.get(*id))
        .cloned()
        .collect();
    diff
}

/// Refines `report`'s statuses by `report.diff`: an asset that no problem names (`check::
/// with_status` decides that) is `New` or `Changed` when the diff adds or changes it, else stays
/// `Ready`. Its key is read by `entry_of` from the asset's own serialized fields, as the diff
/// reads each manifest entry's, so the two can't disagree.
fn with_diff_status(report: &mut CheckReport) {
    let Some(diff) = &report.diff else {
        return;
    };
    for asset in &mut report.assets {
        if asset.status == Status::Problem {
            continue;
        }
        let entry = entry_of(&serde_json::to_value(&*asset).expect("an asset serializes"));
        if diff.added.contains(&entry) {
            asset.status = Status::New;
        } else if diff.changed.contains(&entry) {
            asset.status = Status::Changed;
        }
    }
}

/// The folder name prefixes of the domain folder `D`'s staging and old bundles.
fn prefixes(domain: &str) -> (String, String) {
    (format!(".{domain}.staging-"), format!(".{domain}.old-"))
}

/// The folder holding `bundle`, and the domain's name, which is `bundle`'s folder name.
fn parts(bundle: &Path) -> (&Path, String) {
    let parent = bundle.parent().unwrap_or(Path::new("/"));
    let domain = bundle
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    (parent, domain)
}

/// Cleans up beside `bundle` after a crash (C15 step 4): deletes every leftover `.D.staging-*`,
/// then deletes every `.D.old-*` when `D/` exists, or renames the only one back to `D/` when it
/// is missing. Several `.D.old-*` without a `D/` are an error naming them, and are left as they
/// are for you to choose one.
pub fn recover(bundle: &Path) -> Result<(), String> {
    let (parent, domain) = parts(bundle);
    let (staging_prefix, old_prefix) = prefixes(&domain);
    let entries = match std::fs::read_dir(parent) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("Could not read {}: {e}", parent.display())),
        Ok(entries) => entries,
    };
    let mut olds = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("Could not read {}: {e}", parent.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&staging_prefix) {
            std::fs::remove_dir_all(entry.path())
                .map_err(|e| format!("Could not delete the leftover staging folder {name}: {e}"))?;
            tracing::info!(
                "Deleted the leftover staging folder {}",
                entry.path().display()
            );
        } else if name.starts_with(&old_prefix) {
            olds.push(entry.path());
        }
    }
    olds.sort();
    if bundle.exists() {
        // A `D/` OpenDAM didn't write may have appeared after a crash: an old bundle may then be
        // the only copy of the last export, so it is kept, and the Check reports `D/` (`previous`)
        if previous(bundle).is_err() {
            return Ok(());
        }
        // The swap finished: the old bundles are leftovers
        for old in olds {
            std::fs::remove_dir_all(&old).map_err(|e| {
                format!(
                    "Could not delete the previous bundle {}: {e}",
                    old.display()
                )
            })?;
            tracing::info!("Deleted the previous bundle {}", old.display());
        }
        return Ok(());
    }
    match olds.as_slice() {
        [] => Ok(()),
        [old] => {
            std::fs::rename(old, bundle).map_err(|e| {
                format!(
                    "Could not restore the previous bundle {} to {}: {e}",
                    old.display(),
                    bundle.display()
                )
            })?;
            tracing::info!(
                "Restored the previous bundle {} after a crash",
                bundle.display()
            );
            Ok(())
        }
        several => {
            let names: Vec<String> = several
                .iter()
                .filter_map(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .collect();
            Err(format!(
                "{domain} is missing from {} and {} earlier bundles are left beside it: {}. Rename the one to keep to {domain} and delete the others",
                parent.display(),
                names.len(),
                names.join(", ")
            ))
        }
    }
}

/// The manifest of the bundle at `bundle`, `None` when there is none. A folder there that holds
/// no manifest OpenDAM wrote is an error, since Export would replace it.
pub fn previous(bundle: &Path) -> Result<Option<Value>, String> {
    if !bundle.exists() {
        return Ok(None);
    }
    let path = bundle.join(MANIFEST);
    let manifest = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|m| m["exporter"]["name"] == "OpenDAM");
    match manifest {
        Some(manifest) => Ok(Some(manifest)),
        None => Err(format!(
            "{} holds no manifest.json that OpenDAM wrote, so it isn't a bundle. Export replaces it: move it away first",
            bundle.display()
        )),
    }
}

fn general(message: String) -> Problem {
    Problem {
        message,
        assets: Vec::new(),
    }
}

/// Builds the bundle of `manifest` and `files` in a new staging folder beside `bundle` (C15
/// steps 1 and 2), creating the folder that holds `bundle` if needed, and validates it. Returns
/// the staging folder, or the problems it found, having deleted it.
pub fn stage(
    bundle: &Path,
    manifest: &Value,
    files: &[BundleFile],
) -> Result<PathBuf, Vec<Problem>> {
    let (parent, domain) = parts(bundle);
    let staging = parent.join(format!("{}{}", prefixes(&domain).0, uuid::Uuid::new_v4()));
    let result = std::fs::create_dir_all(parent)
        .and_then(|()| std::fs::create_dir(&staging))
        .map_err(|e| {
            vec![general(format!(
                "Could not create the staging folder {}: {e}",
                staging.display()
            ))]
        })
        .and_then(|()| fill(&staging, manifest, files))
        .and_then(|()| verify(&staging, files));
    match result {
        Ok(()) => Ok(staging),
        Err(problems) => {
            discard(&staging);
            Err(problems)
        }
    }
}

/// Copies each of `files` into `staging` and writes `manifest` beside them, pretty-printed.
fn fill(staging: &Path, manifest: &Value, files: &[BundleFile]) -> Result<(), Vec<Problem>> {
    let mut problems = Vec::new();
    for file in files {
        let target = staging.join(&file.file);
        let copied = target
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::copy(&file.source, &target));
        if let Err(e) = copied {
            problems.push(Problem {
                message: format!(
                    "{}: it can't be copied into the bundle as {}: {e}",
                    file.asset.file_path, file.file
                ),
                assets: vec![file.asset.clone()],
            });
        }
    }
    let mut text = serde_json::to_string_pretty(manifest).expect("manifest serializes");
    text.push('\n');
    if let Err(e) = std::fs::write(staging.join(MANIFEST), text) {
        problems.push(general(format!("Could not write the manifest: {e}")));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Validates the bundle in `folder` (C15 step 2): its `manifest.json` against the schema and
/// C1's checks, that it holds exactly the files the manifest lists, and that each matches its
/// `sha256`. `files` name the asset of each listed file.
pub fn verify(folder: &Path, files: &[BundleFile]) -> Result<(), Vec<Problem>> {
    let manifest: Value = std::fs::read(folder.join(MANIFEST))
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
        .map_err(|e| vec![general(format!("The staged manifest can't be read: {e}"))])?;
    let mut problems: Vec<Problem> = [
        contract::validate_manifest(&manifest),
        contract::check_rules(&manifest),
    ]
    .into_iter()
    .filter_map(Result::err)
    .flatten()
    .map(|e| general(format!("The staged manifest is invalid at {e}")))
    .collect();

    let asset_of = |file: &str| {
        files
            .iter()
            .find(|f| f.file == file)
            .map(|f| f.asset.clone())
    };
    let named = |file: &str, message: String| Problem {
        assets: asset_of(file).into_iter().collect(),
        message,
    };
    let mut listed = BTreeSet::new();
    for asset in manifest["assets"].as_array().into_iter().flatten() {
        let (Some(file), Some(sha256)) = (asset["file"].as_str(), asset["sha256"].as_str()) else {
            continue;
        };
        listed.insert(file.to_string());
        match hashes::sha256_file(&folder.join(file)) {
            Ok(found) if found == sha256 => {}
            Ok(found) => problems.push(named(
                file,
                format!("The staged {file} has the sha256 {found}, not the {sha256} the manifest lists: its source changed while it was read. Check again"),
            )),
            Err(e) => problems.push(named(file, format!("The staged {file} can't be read: {e}"))),
        }
    }
    let mut held = BTreeSet::new();
    for entry in walkdir::WalkDir::new(folder).min_depth(1) {
        let entry =
            entry.map_err(|e| vec![general(format!("The staging folder can't be read: {e}"))])?;
        if entry.file_type().is_dir() {
            continue;
        }
        let relative = entry.path().strip_prefix(folder).expect("below the folder");
        let relative = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if relative != MANIFEST {
            held.insert(relative);
        }
    }
    for extra in held.difference(&listed) {
        problems.push(general(format!(
            "The staged bundle holds {extra}, which the manifest doesn't list"
        )));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Deletes a staging folder, logging a failure: the next Check deletes what is left.
fn discard(staging: &Path) {
    match std::fs::remove_dir_all(staging) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            "Could not delete the staging folder {}: {e}",
            staging.display()
        ),
    }
}

/// Swaps `staging` in as `bundle` (C15 step 3) through `rename`: renames an existing `bundle` to
/// `.D.old-<uuid>`, `staging` to `bundle`, then deletes the old folder. When the second rename
/// fails, the old folder is renamed back.
pub fn swap(
    bundle: &Path,
    staging: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), String> {
    let (parent, domain) = parts(bundle);
    let old = if bundle.exists() {
        let old = parent.join(format!("{}{}", prefixes(&domain).1, uuid::Uuid::new_v4()));
        rename(bundle, &old).map_err(|e| {
            format!(
                "Could not move the previous bundle {} aside: {e}",
                bundle.display()
            )
        })?;
        Some(old)
    } else {
        None
    };
    if let Err(e) = rename(staging, bundle) {
        if let Some(old) = &old {
            if let Err(back) = rename(old, bundle) {
                return Err(format!(
                    "Could not move the checked bundle to {}: {e}. The previous bundle is in {}, which the next Check puts back ({back})",
                    bundle.display(),
                    old.display()
                ));
            }
        }
        return Err(format!(
            "Could not move the checked bundle to {}: {e}. The previous bundle is kept",
            bundle.display()
        ));
    }
    if let Some(old) = old {
        // The next Check deletes it if this fails
        if let Err(e) = std::fs::remove_dir_all(&old) {
            tracing::warn!(
                "Could not delete the previous bundle {}: {e}",
                old.display()
            );
        }
    }
    Ok(())
}

/// A Check's staged bundle, waiting for Export.
struct Staged {
    bundle: PathBuf,
    staging: PathBuf,
    fingerprint: Fingerprint,
    /// The manifest of the bundle the Check diffed against, which Export must still find there
    previous: Option<Value>,
    assets: usize,
}

/// What Export did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum ExportOutcome {
    /// The bundle at `bundle_path` now holds the checked domain
    #[serde(rename_all = "camelCase")]
    Exported { bundle_path: String, assets: usize },
    /// Nothing was written, since `changes` changed since the Check, or nothing was staged:
    /// the staged bundle is discarded, and a new Check is needed
    Stale { changes: Vec<String> },
}

/// The staged bundles of the open Export views, by domain id. Its lock serializes every Check
/// and Export, so no two touch one domain's folder at once. It owns what material inference has
/// read from files for the session, which every Check and Export's re-read go through.
#[derive(Default)]
pub struct Exports {
    staged: Mutex<HashMap<String, Staged>>,
    memo: Arc<infer::Memo>,
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())
}

impl Exports {
    /// What material inference has read from files this session, for a Check run elsewhere,
    /// such as the Game tab's
    pub fn memo(&self) -> &Arc<infer::Memo> {
        &self.memo
    }

    /// Checks the domain `domain_id` (C14), reading FBX facts with `facts`: recovers its folder
    /// after a crash, diffs the bundle against the previous one, and, when nothing refuses the
    /// export, stages it. A domain's earlier staged bundle is discarded first.
    pub async fn check(
        &self,
        db: &Database,
        domain_id: &str,
        facts: &impl fbx_facts::Reader,
    ) -> Result<CheckReport, String> {
        let mut staged = self.staged.lock().await;
        if let Some(earlier) = staged.remove(domain_id) {
            blocking(move || discard(&earlier.staging)).await?;
        }
        let Inspection {
            mut report,
            files,
            fingerprint,
            ..
        } = check::inspect(db, domain_id, facts, &self.memo).await?;
        let Some(bundle) = report.bundle_path.clone().map(PathBuf::from) else {
            return Ok(report);
        };

        let manifest = report.manifest.clone();
        let refused = !report.problems.is_empty();
        let target = bundle.clone();
        let (previous, problems, staging) = blocking(move || {
            let mut problems = Vec::new();
            if let Err(e) = recover(&target) {
                problems.push(general(e));
            }
            let previous = previous(&target).unwrap_or_else(|e| {
                problems.push(general(e));
                None
            });
            let mut staging = None;
            if !refused && problems.is_empty() {
                match stage(&target, &manifest, &files) {
                    Ok(folder) => staging = Some((folder, files.len())),
                    Err(mut found) => problems.append(&mut found),
                }
            }
            (previous, problems, staging)
        })
        .await?;

        report.diff = Some(diff(previous.as_ref(), &report.manifest));
        let checked_previous = previous;
        report.problems.extend(problems);
        report.assets = check::with_status(std::mem::take(&mut report.assets), &report.problems);
        with_diff_status(&mut report);
        if let Some((staging, assets)) = staging {
            report.staged = true;
            staged.insert(
                domain_id.to_string(),
                Staged {
                    bundle,
                    staging,
                    fingerprint,
                    previous: checked_previous,
                    assets,
                },
            );
        }
        Ok(report)
    }

    /// Swaps the domain's staged bundle in (C15 step 3), once a fresh read of everything the
    /// Check read finds no change (C14), and the domain's folder still holds the bundle the Check
    /// diffed against, or still holds nothing. On a change, the staged bundle is discarded and
    /// nothing is written or deleted.
    pub async fn export(
        &self,
        db: &Database,
        domain_id: &str,
        facts: &impl fbx_facts::Reader,
    ) -> Result<ExportOutcome, String> {
        let mut staged = self.staged.lock().await;
        let Some(checked) = staged.remove(domain_id) else {
            return Ok(ExportOutcome::Stale {
                changes: vec!["nothing is checked: the Check was discarded or never run".into()],
            });
        };
        let now = check::inspect(db, domain_id, facts, &self.memo).await;
        let mut changes = match &now {
            Ok(now) => checked.fingerprint.changes(&now.fingerprint),
            Err(e) => vec![e.clone()],
        };
        if let Ok(now) = &now {
            if changes.is_empty() {
                changes = now
                    .report
                    .problems
                    .iter()
                    .map(|p| p.message.clone())
                    .collect();
            }
        }
        if !changes.is_empty() {
            blocking(move || discard(&checked.staging)).await?;
            return Ok(ExportOutcome::Stale { changes });
        }

        let Staged {
            bundle,
            staging,
            previous: checked_previous,
            assets,
            ..
        } = checked;
        let path = bundle.display().to_string();
        let swapped = blocking(move || {
            // Whatever is at `D/` now is what the swap deletes
            if !matches!(previous(&bundle), Ok(now) if now == checked_previous) {
                discard(&staging);
                return Ok(false);
            }
            let swapped = swap(&bundle, &staging, |from, to| std::fs::rename(from, to));
            discard(&staging);
            swapped.map(|()| true)
        })
        .await??;
        if !swapped {
            return Ok(ExportOutcome::Stale {
                changes: vec![format!(
                    "{path} no longer holds the bundle the Check compared against"
                )],
            });
        }
        tracing::info!("Exported {assets} assets to {path}");
        Ok(ExportOutcome::Exported {
            bundle_path: path,
            assets,
        })
    }

    /// Discards the domain's staged bundle, as when its Export view closes.
    pub async fn discard(&self, domain_id: &str) -> Result<(), String> {
        if let Some(checked) = self.staged.lock().await.remove(domain_id) {
            blocking(move || discard(&checked.staging)).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::insert_collection;
    use crate::export::test_fixture::*;
    use serde_json::json;

    /// FBX facts for these tests' domains, which hold no FBX.
    struct NoFbx;

    impl fbx_facts::Reader for NoFbx {
        async fn read(&self, source: &Path, _: &Path) -> Result<fbx_facts::FbxFacts, String> {
            panic!("read {} for its facts", source.display())
        }
    }

    const TEXTURES: [&str; 4] = [
        "Dungeon Props/Textures/T_StoneFloor_D.png",
        "Dungeon Props/Textures/T_StoneFloor_N.png",
        "Kenney Crates/crate_wood.png",
        "Unity Moss/T_Moss_D.png",
    ];

    fn bundle_dir(f: &Fixture) -> PathBuf {
        f.export_root.join("DungeonGame/Props")
    }

    /// Every file below `dir`, relative to it.
    fn listing(dir: &Path) -> Vec<String> {
        let mut files: Vec<String> = walkdir::WalkDir::new(dir)
            .min_depth(1)
            .into_iter()
            .map(|e| e.unwrap())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().strip_prefix(dir).unwrap().display().to_string())
            .collect();
        files.sort();
        files
    }

    /// The folders beside the domain's folder that a check or export leaves.
    fn leftovers(f: &Fixture) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(f.export_root.join("DungeonGame"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        names.sort();
        names
    }

    /// The bundle at `dir` holds `manifest.json` and exactly the files it lists, the manifest
    /// passes the schema and C1's checks, and every file matches its sha256. Returns the manifest.
    fn assert_bundle(dir: &Path) -> Value {
        assert_eq!(verify(dir, &[]), Ok(()));
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(dir.join(MANIFEST)).unwrap()).unwrap();
        let mut listed: Vec<String> = manifest["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["file"].as_str().unwrap().to_string())
            .collect();
        listed.push(MANIFEST.into());
        listed.sort();
        assert_eq!(listing(dir), listed);
        manifest
    }

    async fn domain(f: &Fixture, paths: &[&str]) {
        collection(f, "props", "Props", "dg", paths).await;
    }

    fn names(entries: &[DiffEntry]) -> Vec<(&str, Option<&str>)> {
        entries
            .iter()
            .map(|e| (e.asset_name.as_str(), e.role.as_deref()))
            .collect()
    }

    fn statuses(report: &CheckReport) -> Vec<(&str, Status)> {
        report
            .assets
            .iter()
            .map(|a| (a.asset.file_path.as_str(), a.status))
            .collect()
    }

    async fn export(f: &Fixture, exports: &Exports) -> ExportOutcome {
        exports.export(&f.db, "props", &NoFbx).await.unwrap()
    }

    #[tokio::test]
    async fn check_stages_the_bundle_and_export_swaps_it_in() {
        let f = fixture().await;
        domain(&f, &TEXTURES).await;
        let exports = Exports::default();

        let report = exports.check(&f.db, "props", &NoFbx).await.unwrap();
        assert_eq!(report.problems, []);
        assert!(report.staged);
        let diff = report.diff.as_ref().unwrap();
        assert_eq!(diff.previous_exported_at, None);
        assert_eq!(
            names(&diff.added),
            [
                ("CrateWood", Some("BaseColor")),
                ("Moss", Some("BaseColor")),
                ("StoneFloor", Some("BaseColor")),
                ("StoneFloor", Some("Normal")),
            ]
        );
        assert!(diff.changed.is_empty() && diff.removed.is_empty());
        assert!(report.assets.iter().all(|a| a.status == Status::New));

        // Staged beside the domain's folder, which doesn't exist yet
        let staged = leftovers(&f);
        assert_eq!(staged.len(), 1);
        assert!(staged[0].starts_with(".Props.staging-"), "{staged:?}");
        let staging = f.export_root.join("DungeonGame").join(&staged[0]);
        assert_eq!(assert_bundle(&staging), report.manifest);
        assert!(!bundle_dir(&f).exists());

        // Pretty-printed, with a local exportedAt and the app's version
        let text = std::fs::read_to_string(staging.join(MANIFEST)).unwrap();
        let pretty = serde_json::to_string_pretty(&report.manifest).unwrap();
        assert_eq!(text, format!("{pretty}\n"));
        assert!(text.starts_with("{\n  \"assets\": [\n    {\n"), "{text}");
        let exported_at = report.manifest["exportedAt"].as_str().unwrap();
        let parsed = chrono::DateTime::parse_from_rfc3339(exported_at).unwrap();
        assert_eq!(
            parsed.offset().local_minus_utc(),
            chrono::Local::now().offset().local_minus_utc()
        );
        assert_eq!(
            report.manifest["exporter"],
            json!({"name": "OpenDAM", "version": env!("CARGO_PKG_VERSION")})
        );

        assert_eq!(
            export(&f, &exports).await,
            ExportOutcome::Exported {
                bundle_path: bundle_dir(&f).display().to_string(),
                assets: 4
            }
        );
        let manifest = assert_bundle(&bundle_dir(&f));
        assert_eq!(manifest, report.manifest);
        assert_eq!(
            listing(&bundle_dir(&f)),
            [
                "Textures/CrateWood_BaseColor.png",
                "Textures/Moss_BaseColor.png",
                "Textures/StoneFloor_BaseColor.png",
                "Textures/StoneFloor_Normal.png",
                "manifest.json",
            ]
        );
        let source = f.library.root_path.join(TEXTURES[0]);
        assert_eq!(
            std::fs::read(bundle_dir(&f).join("Textures/StoneFloor_BaseColor.png")).unwrap(),
            std::fs::read(source).unwrap()
        );
        assert_eq!(leftovers(&f), Vec::<String>::new());

        // A second export of the unchanged domain replaces the first with the same files
        let again = exports.check(&f.db, "props", &NoFbx).await.unwrap();
        let diff = again.diff.as_ref().unwrap();
        assert_eq!(diff.previous_exported_at.as_deref(), Some(exported_at));
        assert!(diff.added.is_empty() && diff.changed.is_empty() && diff.removed.is_empty());
        assert!(again.assets.iter().all(|a| a.status == Status::Ready));
        assert!(matches!(
            export(&f, &exports).await,
            ExportOutcome::Exported { .. }
        ));
        assert_bundle(&bundle_dir(&f));
        assert_eq!(leftovers(&f), Vec::<String>::new());
    }

    #[tokio::test]
    async fn the_diff_lists_added_changed_and_removed_assets_and_their_status() {
        let f = fixture().await;
        domain(&f, &TEXTURES[..3]).await;
        let exports = Exports::default();
        exports.check(&f.db, "props", &NoFbx).await.unwrap();
        assert!(matches!(
            export(&f, &exports).await,
            ExportOutcome::Exported { .. }
        ));

        // The crate's pixels change, the moss joins and the stone floor leaves. A member brings
        // in its whole texture set, so its base color and normal map leave together
        std::fs::write(
            f.library.root_path.join("Kenney Crates/crate_wood.png"),
            std::fs::read(f.library.root_path.join("Foo A/tile_a.png")).unwrap(),
        )
        .unwrap();
        add_members(&f.db, &f.library, "props", &[TEXTURES[3]]).await;
        for texture in &TEXTURES[..2] {
            sqlx::query("DELETE FROM collection_assets WHERE asset_id = ?")
                .bind(id_of(&f.library.pool, texture).await)
                .execute(&f.library.pool)
                .await
                .unwrap();
        }

        let report = exports.check(&f.db, "props", &NoFbx).await.unwrap();
        let diff = report.diff.as_ref().unwrap();
        assert_eq!(names(&diff.added), [("Moss", Some("BaseColor"))]);
        assert_eq!(names(&diff.changed), [("CrateWood", Some("BaseColor"))]);
        assert_eq!(
            names(&diff.removed),
            [
                ("StoneFloor", Some("BaseColor")),
                ("StoneFloor", Some("Normal"))
            ]
        );
        assert_eq!(
            statuses(&report),
            [
                ("Kenney Crates/crate_wood.png", Status::Changed),
                ("Unity Moss/T_Moss_D.png", Status::New),
            ]
        );
        assert_eq!(
            json!(diff.removed[1]),
            json!({"kind": "texture", "assetName": "StoneFloor", "role": "Normal"})
        );
    }

    #[tokio::test]
    async fn a_failed_check_keeps_the_previous_bundle_and_stages_nothing() {
        let f = fixture().await;
        domain(&f, &TEXTURES).await;
        let exports = Exports::default();
        exports.check(&f.db, "props", &NoFbx).await.unwrap();
        export(&f, &exports).await;
        let before = std::fs::read(bundle_dir(&f).join(MANIFEST)).unwrap();

        add_members(
            &f.db,
            &f.library,
            "props",
            &["Dungeon Props/Textures/T_Sky.exr"],
        )
        .await;
        let report = exports.check(&f.db, "props", &NoFbx).await.unwrap();

        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(!report.staged);
        // The diff leaves the others ready, and the refused asset a problem
        assert_eq!(
            statuses(&report),
            [
                ("Kenney Crates/crate_wood.png", Status::Ready),
                ("Unity Moss/T_Moss_D.png", Status::Ready),
                ("Dungeon Props/Textures/T_Sky.exr", Status::Problem),
                (TEXTURES[0], Status::Ready),
                (TEXTURES[1], Status::Ready),
            ]
        );
        assert_eq!(leftovers(&f), Vec::<String>::new());
        assert_eq!(
            std::fs::read(bundle_dir(&f).join(MANIFEST)).unwrap(),
            before
        );
        assert_bundle(&bundle_dir(&f));
        // The refused asset doesn't block the diff of the others
        assert!(report.diff.unwrap().added.is_empty());
        assert!(matches!(
            export(&f, &exports).await,
            ExportOutcome::Stale { .. }
        ));
        assert_eq!(
            std::fs::read(bundle_dir(&f).join(MANIFEST)).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn a_crash_between_the_renames_is_recovered_at_the_next_check() {
        let f = fixture().await;
        domain(&f, &TEXTURES).await;
        let exports = Exports::default();
        exports.check(&f.db, "props", &NoFbx).await.unwrap();
        export(&f, &exports).await;
        let files = listing(&bundle_dir(&f));

        // A crash after the first rename, with a staging folder left too
        let parent = f.export_root.join("DungeonGame");
        std::fs::rename(bundle_dir(&f), parent.join(".Props.old-crashed")).unwrap();
        std::fs::create_dir_all(parent.join(".Props.staging-crashed/Textures")).unwrap();
        std::fs::write(parent.join(".Props.staging-crashed/Textures/x.png"), "x").unwrap();

        let report = exports.check(&f.db, "props", &NoFbx).await.unwrap();
        assert_eq!(report.problems, []);
        assert_eq!(listing(&bundle_dir(&f)), files);
        assert_bundle(&bundle_dir(&f));
        // The diff reads the restored manifest
        let diff = report.diff.as_ref().unwrap();
        assert!(diff.previous_exported_at.is_some() && diff.added.is_empty());
        let left = leftovers(&f);
        assert_eq!(left.len(), 1, "only the new staging folder: {left:?}");
        assert!(left[0].starts_with(".Props.staging-") && left[0] != ".Props.staging-crashed");
    }

    #[tokio::test]
    async fn old_bundles_are_deleted_beside_the_bundle_and_several_without_it_are_a_problem() {
        let f = fixture().await;
        domain(&f, &TEXTURES).await;
        let exports = Exports::default();
        exports.check(&f.db, "props", &NoFbx).await.unwrap();
        export(&f, &exports).await;
        let parent = f.export_root.join("DungeonGame");
        // Another domain's folders are never touched
        for name in [".Props.old-a", ".PropsTwo.old-b", ".PropsTwo.staging-c"] {
            std::fs::create_dir(parent.join(name)).unwrap();
        }

        exports.check(&f.db, "props", &NoFbx).await.unwrap();
        exports.discard("props").await.unwrap();
        assert_eq!(leftovers(&f), [".PropsTwo.old-b", ".PropsTwo.staging-c"]);
        assert!(bundle_dir(&f).is_dir());

        std::fs::rename(bundle_dir(&f), parent.join(".Props.old-a")).unwrap();
        std::fs::create_dir(parent.join(".Props.old-b")).unwrap();
        let report = exports.check(&f.db, "props", &NoFbx).await.unwrap();
        assert!(!report.staged);
        assert_eq!(report.problems.len(), 1);
        assert!(
            report.problems[0].message.starts_with(&format!(
                "Props is missing from {} and 2 earlier bundles are left beside it: .Props.old-a, .Props.old-b.",
                parent.display()
            )),
            "{}",
            report.problems[0].message
        );
        assert!(!bundle_dir(&f).exists());
        assert_eq!(
            leftovers(&f),
            [
                ".Props.old-a",
                ".Props.old-b",
                ".PropsTwo.old-b",
                ".PropsTwo.staging-c"
            ]
        );
    }

    #[tokio::test]
    async fn export_leaves_a_domain_folder_that_changed_since_the_check_alone() {
        let f = fixture().await;
        domain(&f, &TEXTURES).await;
        let exports = Exports::default();

        // A folder of yours appears where no bundle was
        assert!(exports.check(&f.db, "props", &NoFbx).await.unwrap().staged);
        std::fs::create_dir_all(bundle_dir(&f)).unwrap();
        std::fs::write(bundle_dir(&f).join("notes.txt"), "mine").unwrap();
        let ExportOutcome::Stale { changes } = export(&f, &exports).await else {
            panic!("exported over a folder of yours");
        };
        assert!(changes[0].ends_with("no longer holds the bundle the Check compared against"));
        assert_eq!(listing(&bundle_dir(&f)), ["notes.txt"]);
        assert_eq!(leftovers(&f), Vec::<String>::new());
        std::fs::remove_dir_all(bundle_dir(&f)).unwrap();

        // The previous bundle is replaced by another export after the Check
        exports.check(&f.db, "props", &NoFbx).await.unwrap();
        export(&f, &exports).await;
        assert!(exports.check(&f.db, "props", &NoFbx).await.unwrap().staged);
        let manifest = bundle_dir(&f).join(MANIFEST);
        let mut other: Value = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        other["exportedAt"] = json!("2026-10-23T09:00:00+00:00");
        std::fs::write(&manifest, other.to_string()).unwrap();
        assert!(matches!(
            export(&f, &exports).await,
            ExportOutcome::Stale { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(&manifest).unwrap(),
            other.to_string()
        );
        assert_eq!(leftovers(&f), Vec::<String>::new());
    }

    #[tokio::test]
    async fn two_domains_of_one_name_are_refused_and_touch_no_folder() {
        let f = fixture().await;
        domain(&f, &TEXTURES).await;
        insert_collection(&f.db.master_pool, "props2", "Props", Some("dg")).await;
        let exports = Exports::default();

        for id in ["props", "props2"] {
            let report = exports.check(&f.db, id, &NoFbx).await.unwrap();
            assert_eq!(
                report.problems.iter().map(|p| p.message.as_str()).collect::<Vec<_>>(),
                ["Another domain in DungeonGame is also named Props: each domain is a folder of the bundle path, so rename one"]
            );
            assert_eq!(report.bundle_path, None);
            assert!(!report.staged);
        }
        assert!(!f.export_root.join("DungeonGame").exists());
    }

    #[tokio::test]
    async fn old_bundles_are_kept_while_the_domain_folder_isnt_an_opendam_bundle() {
        let f = fixture().await;
        domain(&f, &TEXTURES).await;
        let exports = Exports::default();
        exports.check(&f.db, "props", &NoFbx).await.unwrap();
        export(&f, &exports).await;
        let files = listing(&bundle_dir(&f));

        // A crash after the first rename, then a folder of yours at the bundle path
        let parent = f.export_root.join("DungeonGame");
        std::fs::rename(bundle_dir(&f), parent.join(".Props.old-crashed")).unwrap();
        std::fs::create_dir(bundle_dir(&f)).unwrap();
        std::fs::write(bundle_dir(&f).join("notes.txt"), "mine").unwrap();

        let report = exports.check(&f.db, "props", &NoFbx).await.unwrap();
        assert_eq!(report.problems.len(), 1);
        assert!(report.problems[0]
            .message
            .contains("holds no manifest.json that OpenDAM wrote"));
        assert_eq!(leftovers(&f), [".Props.old-crashed"]);
        assert_eq!(listing(&parent.join(".Props.old-crashed")), files);

        // Once your folder is moved away, the next Check restores the bundle
        std::fs::remove_dir_all(bundle_dir(&f)).unwrap();
        let report = exports.check(&f.db, "props", &NoFbx).await.unwrap();
        assert_eq!(report.problems, []);
        assert_eq!(listing(&bundle_dir(&f)), files);
    }

    #[tokio::test]
    async fn a_pack_only_change_shows_in_the_diff() {
        let f = fixture().await;
        domain(&f, &TEXTURES).await;
        let exports = Exports::default();
        exports.check(&f.db, "props", &NoFbx).await.unwrap();
        export(&f, &exports).await;

        sqlx::query(
            "INSERT INTO pack_overrides (path, name, updated_at) VALUES ('Kenney Crates', 'Crates', 0)",
        )
        .execute(&f.library.pool)
        .await
        .unwrap();
        crate::structure::rebuild_library(&f.db, &f.library).await;
        let report = exports.check(&f.db, "props", &NoFbx).await.unwrap();
        let diff = report.diff.unwrap();
        assert_eq!(diff.packs, ["kenney-crates"]);
        assert!(diff.added.is_empty() && diff.changed.is_empty() && diff.removed.is_empty());
    }

    #[tokio::test]
    async fn a_folder_that_isnt_an_opendam_bundle_is_never_replaced() {
        let f = fixture().await;
        domain(&f, &TEXTURES).await;
        std::fs::create_dir_all(bundle_dir(&f)).unwrap();
        std::fs::write(bundle_dir(&f).join("notes.txt"), "mine").unwrap();
        let exports = Exports::default();

        let report = exports.check(&f.db, "props", &NoFbx).await.unwrap();
        assert!(!report.staged);
        assert_eq!(report.problems.len(), 1);
        assert!(report.problems[0]
            .message
            .contains("holds no manifest.json that OpenDAM wrote"));
        assert_eq!(listing(&bundle_dir(&f)), ["notes.txt"]);
        assert_eq!(leftovers(&f), Vec::<String>::new());
    }

    #[tokio::test]
    async fn closing_the_view_discards_the_staged_bundle() {
        let f = fixture().await;
        domain(&f, &TEXTURES).await;
        let exports = Exports::default();
        assert!(exports.check(&f.db, "props", &NoFbx).await.unwrap().staged);
        assert_eq!(leftovers(&f).len(), 1);

        exports.discard("props").await.unwrap();
        assert_eq!(leftovers(&f), Vec::<String>::new());
        assert!(matches!(
            export(&f, &exports).await,
            ExportOutcome::Stale { .. }
        ));
        assert!(!bundle_dir(&f).exists());
    }

    /// Makes the change `case` to the fixture, returning the input Export names for it.
    async fn change(f: &Fixture, case: &str) -> &'static str {
        let root = &f.library.root_path;
        let paths = [root.display().to_string()];
        let reindex = || crate::watcher::index_paths(&f.db, "fixture", &paths);
        let master = |query: &'static str| sqlx::query(query).execute(&f.db.master_pool);
        match case {
            "a conversion.json above a texture" => {
                let file = r#"{"contractVersion": 1, "steps": []}"#;
                std::fs::write(root.join("Kenney Crates/conversion.json"), file).unwrap();
                "Kenney Crates/conversion.json in Fixture"
            }
            "a .meta beside a pack file" => {
                std::fs::write(root.join("Kenney Crates/crate_wood.png.meta"), "").unwrap();
                "Kenney Crates/crate_wood.png.meta in Fixture"
            }
            "a new image in a pack" => {
                let pack = root.join("Kenney Crates");
                std::fs::copy(pack.join("crate_wood.png"), pack.join("crate_wood_N.png")).unwrap();
                reindex().await.unwrap();
                "the indexed files of the pack Kenney Crates in Fixture"
            }
            "a source's bytes" => {
                std::fs::write(root.join("Unity Moss/T_Moss_D.png"), "changed").unwrap();
                "Unity Moss/T_Moss_D.png in Fixture"
            }
            "a source touched" => {
                // Same bytes and size, a later mtime
                let file = std::fs::File::options()
                    .write(true)
                    .open(root.join("Unity Moss/T_Moss_D.png"))
                    .unwrap();
                let later = file.metadata().unwrap().modified().unwrap()
                    + std::time::Duration::from_secs(3600);
                file.set_modified(later).unwrap();
                "Unity Moss/T_Moss_D.png in Fixture"
            }
            "a member renamed" => {
                let pack = root.join("Unity Moss");
                std::fs::rename(pack.join("T_Moss_D.png"), pack.join("T_Moss_N.png")).unwrap();
                reindex().await.unwrap();
                "the domain's members in Fixture"
            }
            "an asset renamed in its Game tab" => {
                let id = id_of(&f.library.pool, "Unity Moss/T_Moss_D.png").await;
                let rename = json!({"assetName": "Lichen"});
                crate::export::edits::write(&f.library.pool, &id, rename.as_object().unwrap())
                    .await
                    .unwrap();
                "your export edits in Fixture"
            }
            "a pack override" => {
                sqlx::query(
                    "INSERT INTO pack_overrides (path, name, updated_at) VALUES ('Kenney Crates', 'Crates', 0)",
                )
                .execute(&f.library.pool)
                .await
                .unwrap();
                "the pack Kenney Crates in Fixture"
            }
            "the domain renamed" => {
                master("UPDATE collections SET name = 'Loot' WHERE id = 'props'")
                    .await
                    .unwrap();
                "the domain's name"
            }
            "the root collection renamed" => {
                master("UPDATE collections SET name = 'Game' WHERE id = 'dg'")
                    .await
                    .unwrap();
                "the root collection's name"
            }
            "a subcollection added" => {
                insert_collection(&f.db.master_pool, "sub", "Sub", Some("props")).await;
                "the domain's subcollections"
            }
            "the export root moved" => {
                let other = f.dir.path().join("other-export");
                std::fs::create_dir(&other).unwrap();
                crate::export::settings::update(&f.db.master_pool, json!({"root": other}))
                    .await
                    .unwrap();
                "the export settings"
            }
            _ => unreachable!("{case}"),
        }
    }

    /// Each change between Check and Export makes Export discard the staged bundle and write
    /// nothing, naming what changed.
    #[tokio::test]
    async fn export_asks_for_a_new_check_when_anything_the_check_read_changed() {
        for case in [
            "a conversion.json above a texture",
            "a .meta beside a pack file",
            "a new image in a pack",
            "a source's bytes",
            "a source touched",
            "a member renamed",
            "an asset renamed in its Game tab",
            "a pack override",
            "the domain renamed",
            "the root collection renamed",
            "a subcollection added",
            "the export root moved",
        ] {
            let f = fixture().await;
            domain(&f, &TEXTURES).await;
            let exports = Exports::default();
            assert!(
                exports.check(&f.db, "props", &NoFbx).await.unwrap().staged,
                "{case}"
            );

            let expected = change(&f, case).await;
            let ExportOutcome::Stale { changes } = export(&f, &exports).await else {
                panic!("{case}: exported");
            };
            assert!(changes.iter().any(|c| c == expected), "{case}: {changes:?}");
            assert!(!bundle_dir(&f).exists(), "{case}");
            assert!(!f.export_root.join("Game").exists(), "{case}");
            assert!(!f.export_root.join("DungeonGame/Loot").exists(), "{case}");
            assert!(
                !f.dir.path().join("other-export/DungeonGame").exists(),
                "{case}"
            );
            assert_eq!(leftovers(&f), Vec::<String>::new(), "{case}");
        }
    }

    #[test]
    fn a_failed_second_rename_puts_the_previous_bundle_back() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("Props");
        let staging = dir.path().join(".Props.staging-x");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join(MANIFEST), "previous").unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join(MANIFEST), "next").unwrap();

        let mut calls = 0;
        let failing = |from: &Path, to: &Path| {
            calls += 1;
            if calls == 2 {
                Err(std::io::Error::other("disk full"))
            } else {
                std::fs::rename(from, to)
            }
        };
        let error = swap(&bundle, &staging, failing).unwrap_err();
        assert!(
            error.contains("disk full") && error.ends_with("The previous bundle is kept"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(bundle.join(MANIFEST)).unwrap(),
            "previous"
        );
        assert!(staging.is_dir());
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "no .Props.old-* is left: {names:?}");

        // Without a previous bundle the staging folder is simply renamed
        std::fs::remove_dir_all(&bundle).unwrap();
        swap(&bundle, &staging, |a, b| std::fs::rename(a, b)).unwrap();
        assert_eq!(
            std::fs::read_to_string(bundle.join(MANIFEST)).unwrap(),
            "next"
        );
        assert!(!staging.exists());
    }

    /// A valid manifest listing one texture, `Textures/Rock_BaseColor.png` with the sha256
    /// `sha256`, and that texture's bundle file, copied from `source`.
    fn rock_bundle(sha256: &str, source: PathBuf) -> (Value, BundleFile) {
        let manifest = json!({
            "contractVersion": 1, "domain": "Props", "exportedAt": "2026-10-22T12:00:00+02:00",
            "exporter": {"name": "OpenDAM", "version": "0.1.0"},
            "packs": [{"id": "pack", "name": "Pack", "licence": {"spdx": null, "name": "Unknown"}, "origin": "generic"}],
            "assets": [{
                "kind": "texture", "assetName": "Rock", "file": "Textures/Rock_BaseColor.png",
                "sha256": sha256, "pack": "pack", "sourcePath": "rock.png",
                "conversion": [], "role": "BaseColor"
            }]
        });
        let file = BundleFile {
            file: "Textures/Rock_BaseColor.png".into(),
            source,
            sha256: sha256.into(),
            asset: check::AssetRef {
                library_id: "lib".into(),
                asset_id: "rock".into(),
                file_path: "Pack/rock.png".into(),
            },
        };
        (manifest, file)
    }

    #[test]
    fn verify_refuses_unlisted_files_and_wrong_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();
        std::fs::create_dir(folder.join("Textures")).unwrap();
        std::fs::write(folder.join("Textures/Rock_BaseColor.png"), "rock").unwrap();
        std::fs::write(folder.join("Textures/Extra.png"), "extra").unwrap();
        let (manifest, file) = rock_bundle(&"0".repeat(64), folder.join("unused"));
        std::fs::write(folder.join(MANIFEST), manifest.to_string()).unwrap();
        let rock = file.asset.clone();
        let files = [file];

        let problems = verify(folder, &files).unwrap_err();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0]
            .message
            .starts_with("The staged Textures/Rock_BaseColor.png has the sha256 "));
        assert_eq!(problems[0].assets, [rock]);
        assert_eq!(
            problems[1].message,
            "The staged bundle holds Textures/Extra.png, which the manifest doesn't list"
        );
    }

    /// A check that fails while staging, copying or validating, deletes its staging folder and
    /// leaves the previous bundle as it was.
    #[test]
    fn a_failed_staging_deletes_the_staging_folder() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("DungeonGame");
        let bundle = parent.join("Props");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join(MANIFEST), "previous").unwrap();
        let source = dir.path().join("rock.png");
        std::fs::write(&source, "rock").unwrap();
        let folders = || {
            let mut names: Vec<String> = std::fs::read_dir(&parent)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };

        // The copy fails: the source is gone
        let (manifest, missing) = rock_bundle(&"0".repeat(64), dir.path().join("gone.png"));
        let problems = stage(&bundle, &manifest, &[missing]).unwrap_err();
        assert!(
            problems[0]
                .message
                .starts_with("Pack/rock.png: it can't be copied into the bundle as Textures/Rock_BaseColor.png: "),
            "{problems:?}"
        );
        assert_eq!(problems[0].assets[0].asset_id, "rock");
        assert_eq!(folders(), ["Props"]);

        // Validation fails: the copied file doesn't match the manifest's sha256
        let (manifest, changed) = rock_bundle(&"0".repeat(64), source.clone());
        let problems = stage(&bundle, &manifest, &[changed]).unwrap_err();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0]
            .message
            .starts_with("The staged Textures/Rock_BaseColor.png has the sha256 "));
        assert_eq!(folders(), ["Props"]);

        // Validation fails: the manifest breaks the schema
        let sha256 = hashes::sha256_file(&source).unwrap();
        let (mut manifest, file) = rock_bundle(&sha256, source.clone());
        manifest["domain"] = json!("not pascal");
        let problems = stage(&bundle, &manifest, std::slice::from_ref(&file)).unwrap_err();
        assert!(
            problems[0]
                .message
                .starts_with("The staged manifest is invalid at /domain: "),
            "{problems:?}"
        );
        assert_eq!(folders(), ["Props"]);
        assert_eq!(
            std::fs::read_to_string(bundle.join(MANIFEST)).unwrap(),
            "previous"
        );

        // With all of it right, the staging folder stays for Export
        let (manifest, _) = rock_bundle(&sha256, source);
        let staging = stage(&bundle, &manifest, &[file]).unwrap();
        assert!(staging.starts_with(&parent));
        assert_eq!(folders().len(), 2);
        assert_eq!(verify(&staging, &[]), Ok(()));
    }

    #[test]
    fn the_diff_compares_entries_by_kind_name_and_role() {
        let texture = |name: &str, role: &str, sha: &str| json!({"kind": "texture", "assetName": name, "role": role, "sha256": sha});
        let previous = json!({"exportedAt": "then", "assets": [
            texture("Rock", "BaseColor", "a"), texture("Rock", "Normal", "b"), texture("Old", "BaseColor", "c")
        ]});
        let next = json!({"assets": [
            texture("Rock", "BaseColor", "a"), texture("Rock", "Normal", "B"), texture("New", "BaseColor", "c")
        ]});

        let d = diff(Some(&previous), &next);
        assert_eq!(d.previous_exported_at.as_deref(), Some("then"));
        assert_eq!(names(&d.added), [("New", Some("BaseColor"))]);
        assert_eq!(names(&d.changed), [("Rock", Some("Normal"))]);
        assert_eq!(names(&d.removed), [("Old", Some("BaseColor"))]);
        assert_eq!(names(&diff(None, &next).added).len(), 3);
        assert!(d.packs.is_empty());

        // Packs are compared by id: an edited entry, a new one and one that left
        let pack = |id: &str, name: &str| json!({"id": id, "name": name});
        let mut previous = previous;
        previous["packs"] = json!([pack("crates", "Crates"), pack("gone", "Gone")]);
        let mut next = next;
        next["packs"] = json!([pack("crates", "Crate Kit"), pack("moss", "Moss")]);
        assert_eq!(
            diff(Some(&previous), &next).packs,
            ["crates", "gone", "moss"]
        );
    }
}
