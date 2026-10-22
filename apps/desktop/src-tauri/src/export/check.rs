//! The Export view's Check (plan game-export C14): reads a domain, builds its manifest in memory,
//! validates it, and lists every asset with its kind, default `assetName`, pack and status, and
//! every problem that would refuse the export.
//!
//! A domain is a direct child of the root collection (C3). Its assets are the grid items in it
//! and all its subcollections, across every library, each once. Until later steps of the plan's
//! build order land, only standalone textures that pass through byte for byte export: the guards
//! refuse a texture under a `conversion.json`, a texture that needs converting, and any FBX.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;
use serde_json::{json, Value};

use crate::db::{Database, LibraryHandle};
use crate::export::conversion::Walker;
use crate::export::names::{is_pascal, pascal};
use crate::export::packs::{self, PackSource};
use crate::export::textures::{self, Convention, Layout, Refusal, Role};
use crate::export::{contract, hashes, settings};
use crate::models::AssetKind;
use crate::structure::{self, PackOverride};

/// One asset file a check names: in a listing, a problem or the Game tab's link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetRef {
    pub library_id: String,
    pub asset_id: String,
    /// Relative to the library root
    pub file_path: String,
}

/// Something that refuses the export, with the assets it is about, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub message: String,
    pub assets: Vec<AssetRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Model,
    Texture,
    Animation,
    /// A member that is neither a model, an animation nor an image (A6)
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ready,
    Problem,
}

/// One row of the Export view's list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckedAsset {
    #[serde(flatten)]
    pub asset: AssetRef,
    pub kind: Kind,
    /// Its default name (C5), `None` when it has none
    pub asset_name: Option<String>,
    pub role: Option<Role>,
    /// Its pack's id, `None` for an asset in no pack
    pub pack: Option<String>,
    pub status: Status,
}

/// What a Check found.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckReport {
    pub domain: String,
    /// `<export root>/<root collection>/<domain>`, once both settings are set
    pub bundle_path: Option<String>,
    pub assets: Vec<CheckedAsset>,
    /// Every pack an asset of the domain is in, in manifest order
    pub packs: Vec<DomainPack>,
    pub problems: Vec<Problem>,
    /// The manifest of the assets that are ready, as the bundle would hold it
    pub manifest: Value,
}

/// A pack of the domain: its manifest entry, where it is, and its inferred normal-map
/// convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DomainPack {
    #[serde(flatten)]
    pub pack: packs::ExportPack,
    pub library_id: String,
    /// The pack folder, relative to the library root
    pub path: String,
    pub normal_convention: Convention,
}

/// A grid item in the domain, as its library DB holds it.
#[derive(Debug, Clone, sqlx::FromRow)]
struct ItemRow {
    id: String,
    file_path: String,
    file_name: String,
    extension: String,
    kind: String,
}

/// A pack folder of a library, with your edits.
#[derive(Debug, Clone)]
struct PackFolder {
    path: String,
    name: String,
    info: structure::PackInfo,
    edits: PackOverride,
}

/// What a check reads from one library's DB before it touches a file.
struct LibraryInput {
    library: LibraryHandle,
    items: Vec<ItemRow>,
    /// The main items whose group has an FBX member
    groups_with_fbx: HashSet<String>,
    /// The pack at or above each item, by its index in `items`
    item_packs: Vec<Option<usize>>,
    packs: Vec<PackFolder>,
    /// The indexed files of each pack an item is in, by its index in `packs`
    pack_files: HashMap<usize, Vec<String>>,
    memo: hashes::Memo,
}

/// One message per general problem, before any asset is read.
fn general(message: impl Into<String>) -> Problem {
    Problem {
        message: message.into(),
        assets: Vec::new(),
    }
}

/// Checks the domain collection `domain_id`. An unknown collection is an error; everything else
/// that would refuse the export is a problem in the report.
pub async fn check(db: &Database, domain_id: &str) -> Result<CheckReport, String> {
    let master = &db.master_pool;
    let settings = settings::load(master).await.map_err(|e| e.to_string())?;
    let collections: Vec<(String, String, Option<String>, bool)> =
        sqlx::query_as("SELECT id, name, parent_id, is_smart FROM collections")
            .fetch_all(master)
            .await
            .map_err(|e| e.to_string())?;
    let find = |id: &str| collections.iter().find(|c| c.0 == id);
    let (_, domain, domain_parent, _) = find(domain_id)
        .ok_or_else(|| format!("No collection has the id {domain_id}"))?
        .clone();

    let mut problems = Vec::new();
    match &settings.root {
        None => problems.push(general(
            "Set the export root in Settings, under Game export",
        )),
        Some(root) => {
            if let Err(e) = settings::check_root(master, root).await {
                problems.push(general(e));
            }
        }
    }
    let root_collection = settings.root_collection.as_deref().map(|id| (id, find(id)));
    let root_name = match root_collection {
        None => {
            problems.push(general(
                "Set the root collection in Settings, under Game export",
            ));
            None
        }
        Some((id, None)) => {
            problems.push(general(format!(
                "The root collection {id} set in Settings no longer exists"
            )));
            None
        }
        Some((id, Some((_, name, _, _)))) => {
            if domain_parent.as_deref() != Some(id) {
                problems.push(general(format!(
                    "{domain} is not a domain: domains are the collections directly inside the root collection {name}"
                )));
            }
            if !is_pascal(name) {
                problems.push(general(format!(
                    "The root collection's name {name} isn't PascalCase (a capital letter, then letters and digits): it is a folder of the bundle path"
                )));
            }
            Some(name.clone())
        }
    };
    if !is_pascal(&domain) {
        problems.push(general(format!(
            "The domain's name {domain} isn't PascalCase (a capital letter, then letters and digits): it is a folder of the bundle path and the manifest's domain"
        )));
    }

    // The domain and every collection below it, parents first
    let mut subtree = vec![domain_id.to_string()];
    let mut i = 0;
    while i < subtree.len() {
        let parent = subtree[i].clone();
        let children: Vec<String> = collections
            .iter()
            .filter(|c| c.2.as_deref() == Some(&parent) && !subtree.contains(&c.0))
            .map(|c| c.0.clone())
            .collect();
        subtree.extend(children);
        i += 1;
    }
    for id in &subtree {
        if let Some((_, name, _, true)) = find(id) {
            problems.push(general(format!(
                "{name} is a smart collection: a domain and its subcollections must be ordinary collections"
            )));
        }
    }

    let bundle_path = match (&settings.root, &root_name) {
        (Some(root), Some(root_name)) => Some(
            Path::new(root)
                .join(root_name)
                .join(&domain)
                .display()
                .to_string(),
        ),
        _ => None,
    };

    let mut inputs = Vec::new();
    for library in db.get_all_libraries().await {
        inputs.push(
            read_library(library, &subtree)
                .await
                .map_err(|e| e.to_string())?,
        );
    }
    let domain_name = domain.clone();
    let (report, inputs) = tokio::task::spawn_blocking(move || {
        let report = evaluate(&domain_name, &mut inputs);
        (report, inputs)
    })
    .await
    .map_err(|e| e.to_string())?;
    for input in &inputs {
        if let Err(e) = hashes::save(&input.library.pool, &input.memo.fresh).await {
            tracing::warn!(
                "Could not save file hashes of the library {}: {e}",
                input.library.name
            );
        }
    }

    let (assets, packs, mut found, manifest) = report;
    problems.append(&mut found);
    let assets = with_status(assets, &problems);
    Ok(CheckReport {
        domain,
        bundle_path,
        assets,
        packs,
        problems,
        manifest,
    })
}

/// The pack at or above `dir` among `packs`, the innermost first.
fn innermost_pack(packs: &HashMap<&str, usize>, dir: &str) -> Option<usize> {
    let mut dir = dir;
    loop {
        if let Some(i) = packs.get(dir) {
            return Some(*i);
        }
        if dir.is_empty() {
            return None;
        }
        dir = dir.rsplit_once('/').map_or("", |(parent, _)| parent);
    }
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

async fn read_library(library: LibraryHandle, subtree: &[String]) -> sqlx::Result<LibraryInput> {
    let pool = &library.pool;
    let ids = serde_json::to_string(subtree).expect("ids serialize");
    // Memberships are on every member of a group, so each is read as its grid item
    let items: Vec<ItemRow> = sqlx::query_as(
        "SELECT a.id, a.file_path, a.file_name, a.extension, a.kind FROM assets a
         WHERE a.id IN (
             SELECT COALESCE(m.group_id, m.id) FROM collection_assets ca
             JOIN assets m ON m.id = ca.asset_id
             WHERE ca.collection_id IN (SELECT value FROM json_each(?)))
         ORDER BY a.file_path",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    let item_ids: Vec<&str> = items.iter().map(|i| i.id.as_str()).collect();
    let groups_with_fbx: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT group_id FROM assets
         WHERE group_id IN (SELECT value FROM json_each(?)) AND lower(extension) = 'fbx'",
    )
    .bind(serde_json::to_string(&item_ids).expect("ids serialize"))
    .fetch_all(pool)
    .await?;

    let overrides = structure::read_overrides(pool).await?;
    let folders: Vec<(String, String, Option<String>)> =
        sqlx::query_as("SELECT path, name, pack_json FROM folders WHERE is_pack ORDER BY path")
            .fetch_all(pool)
            .await?;
    let packs: Vec<PackFolder> = folders
        .into_iter()
        .map(|(path, name, json)| PackFolder {
            edits: overrides.get(&path).cloned().unwrap_or_default(),
            info: structure::parse_pack(json.as_deref()).unwrap_or_default(),
            name: if name.is_empty() {
                library.name.clone()
            } else {
                name
            },
            path,
        })
        .collect();
    let by_path: HashMap<&str, usize> = packs
        .iter()
        .enumerate()
        .map(|(i, p)| (p.path.as_str(), i))
        .collect();
    let item_packs: Vec<Option<usize>> = items
        .iter()
        .map(|item| innermost_pack(&by_path, dir_of(&item.file_path)))
        .collect();

    let mut pack_files = HashMap::new();
    for pack in item_packs.iter().flatten().collect::<HashSet<_>>() {
        let path = &packs[*pack].path;
        let below: Vec<(String,)> = sqlx::query_as(
            "SELECT file_path FROM assets
             WHERE ?1 = '' OR (file_path >= ?1 || '/' AND file_path < ?1 || '0')",
        )
        .bind(path)
        .fetch_all(pool)
        .await?;
        // A nested pack's files are its own
        let files = below
            .into_iter()
            .map(|(f,)| f)
            .filter(|f| innermost_pack(&by_path, dir_of(f)) == Some(*pack))
            .collect();
        pack_files.insert(*pack, files);
    }

    let paths: Vec<String> = items.iter().map(|i| i.file_path.clone()).collect();
    let memo = hashes::load(pool, &paths).await?;
    Ok(LibraryInput {
        groups_with_fbx: groups_with_fbx.into_iter().map(|(id,)| id).collect(),
        library,
        items,
        item_packs,
        packs,
        pack_files,
        memo,
    })
}

/// A texture the check found ready, with what its manifest entry needs.
struct ReadyTexture {
    name: String,
    role: Role,
    extension: &'static str,
    sha256: String,
    source_path: String,
}

/// One asset as the check reads it, before names are compared.
struct Read {
    row: CheckedAsset,
    /// Index of its pack in the domain's packs
    pack: Option<usize>,
    ready: Option<ReadyTexture>,
}

type Evaluated = (Vec<CheckedAsset>, Vec<DomainPack>, Vec<Problem>, Value);

/// Everything a check does with files, run on a blocking thread: reads each item, its pack and
/// its file, compares names, and builds and validates the manifest.
fn evaluate(domain: &str, inputs: &mut [LibraryInput]) -> Evaluated {
    let mut problems = Vec::new();
    let mut reads: Vec<Read> = Vec::new();
    // The domain's packs in manifest order (library order, then path), by (library, pack index)
    let mut pack_order: Vec<(usize, usize)> = Vec::new();
    for (l, input) in inputs.iter().enumerate() {
        let mut used: Vec<usize> = input.item_packs.iter().flatten().copied().collect();
        used.sort_by(|a, b| input.packs[*a].path.cmp(&input.packs[*b].path));
        used.dedup();
        pack_order.extend(used.into_iter().map(|p| (l, p)));
    }
    let mut walkers: Vec<Walker> = inputs
        .iter()
        .map(|i| Walker::new(&i.library.root_path))
        .collect();
    let sources: Vec<PackSource> = pack_order
        .iter()
        .map(|(l, p)| {
            let input = &inputs[*l];
            let pack = &input.packs[*p];
            let root = &input.library.root_path;
            let files = input.pack_files.get(p).map_or(&[][..], Vec::as_slice);
            PackSource {
                library_id: input.library.id.clone(),
                path: pack.path.clone(),
                folder_name: pack.name.clone(),
                package_name: pack
                    .info
                    .package
                    .as_ref()
                    .and_then(|f| packs::read_package_name(root, &f.path)),
                origin: packs::infer_origin(root, files, &mut walkers[*l]),
                info: pack.info.clone(),
                edits: pack.edits.clone(),
            }
        })
        .collect();
    let export_packs = packs::export_packs(&sources);
    let pack_index: HashMap<(usize, usize), usize> = pack_order
        .iter()
        .enumerate()
        .map(|(i, key)| (*key, i))
        .collect();

    let mut seen = HashSet::new();
    for (l, input) in inputs.iter_mut().enumerate() {
        let root = input.library.root_path.clone();
        for (i, item) in input.items.iter().enumerate() {
            if !seen.insert(item.id.clone()) {
                continue;
            }
            let asset = AssetRef {
                library_id: input.library.id.clone(),
                asset_id: item.id.clone(),
                file_path: item.file_path.clone(),
            };
            let pack = input.item_packs[i].map(|p| pack_index[&(l, p)]);
            let mut issue = |message: String| {
                problems.push(Problem {
                    message: format!("{}: {message}", item.file_path),
                    assets: vec![asset.clone()],
                })
            };
            if pack.is_none() {
                issue("it is in no pack: mark its folder as a pack".into());
            }
            let stem = item
                .file_name
                .rsplit_once('.')
                .map_or(item.file_name.as_str(), |(stem, _)| stem);
            let extension = item.extension.to_ascii_lowercase();
            let mut row = CheckedAsset {
                asset: asset.clone(),
                kind: Kind::Other,
                asset_name: None,
                role: None,
                pack: pack.map(|p| export_packs[p].id.clone()),
                status: Status::Ready,
            };
            let mut ready = None;
            match AssetKind::from_str(&item.kind) {
                AssetKind::Model3d => {
                    row.kind = Kind::Model;
                    row.asset_name = Some(pascal(stem));
                    if extension == "fbx" || input.groups_with_fbx.contains(&item.id) {
                        issue("model export not built yet (an FBX exports as a model or an animation, and neither is built yet)".into());
                    } else {
                        issue("it has no FBX file: a model exports from its group's FBX".into());
                    }
                }
                AssetKind::Texture | AssetKind::Image | AssetKind::Hdri => {
                    row.kind = Kind::Texture;
                    let stem_info = textures::read_stem(stem);
                    let layout = stem_info.layout.unwrap_or(Layout::Role(Role::BaseColor));
                    let convention = stem_info.convention.unwrap_or_else(|| {
                        pack.map_or(Convention::OpenGl, |p| {
                            export_packs[p].origin.normal_convention()
                        })
                    });
                    let name = pascal(&stem_info.base);
                    row.asset_name = Some(name.clone());
                    if let Layout::Role(role) = layout {
                        row.role = Some(role);
                    }
                    let mut exportable = pack.is_some();
                    if let Some(nearest) = walkers[l].nearest_for_file(&item.file_path) {
                        issue(format!(
                            "conversion.json support not built yet ({} covers this file)",
                            nearest.path
                        ));
                        exportable = false;
                    }
                    match textures::pass_through(&extension, layout, convention) {
                        Err(Refusal::NeedsConversion(m) | Refusal::Unsupported(m)) => {
                            issue(m);
                            exportable = false;
                        }
                        Ok(pass) if exportable && is_pascal(&name) => {
                            match input.memo.sha256(&root, &item.file_path) {
                                Ok(sha256) => {
                                    let pack_path = &sources[pack.expect("exportable")].path;
                                    ready = Some(ReadyTexture {
                                        name,
                                        role: pass.role,
                                        extension: pass.extension,
                                        sha256,
                                        source_path: relative_to(pack_path, &item.file_path),
                                    })
                                }
                                Err(e) => issue(format!("it can't be read: {e}")),
                            }
                        }
                        Ok(_) => {}
                    }
                }
                _ => issue(
                    "it is neither a model, an animation nor an image, so it can't be exported"
                        .into(),
                ),
            }
            reads.push(Read { row, pack, ready });
        }
    }

    names(&reads, &mut problems);
    let (manifest, mut invalid) = manifest(domain, &reads, &export_packs, &problems);
    problems.append(&mut invalid);
    let mut rows: Vec<CheckedAsset> = reads.into_iter().map(|r| r.row).collect();
    rows.sort_by(|a, b| {
        (a.kind, &a.asset_name, a.role, &a.asset.file_path).cmp(&(
            b.kind,
            &b.asset_name,
            b.role,
            &b.asset.file_path,
        ))
    });
    let domain_packs = export_packs
        .into_iter()
        .zip(&sources)
        .map(|(pack, source)| DomainPack {
            normal_convention: pack.origin.normal_convention(),
            library_id: source.library_id.clone(),
            path: source.path.clone(),
            pack,
        })
        .collect();
    (rows, domain_packs, problems, manifest)
}

/// `file_path` relative to the pack folder `pack_path`, with `/` separators.
fn relative_to(pack_path: &str, file_path: &str) -> String {
    if pack_path.is_empty() {
        file_path.to_string()
    } else {
        file_path
            .strip_prefix(pack_path)
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(file_path)
            .to_string()
    }
}

/// Names that aren't PascalCase, and names that collide (C5): two models or animations with one
/// name, or two textures with one name and role. Each collision is one problem naming every
/// asset in it.
fn names(reads: &[Read], problems: &mut Vec<Problem>) {
    let mut by_key: BTreeMap<(bool, String, Option<Role>), Vec<&CheckedAsset>> = BTreeMap::new();
    for read in reads {
        let row = &read.row;
        let Some(name) = &row.asset_name else {
            continue;
        };
        if !is_pascal(name) {
            problems.push(Problem {
                message: format!(
                    "{}: its file name gives no PascalCase name (a capital letter, then letters and digits)",
                    row.asset.file_path
                ),
                assets: vec![row.asset.clone()],
            });
            continue;
        }
        match row.kind {
            Kind::Texture => by_key
                .entry((true, name.clone(), row.role))
                .or_default()
                .push(row),
            Kind::Model | Kind::Animation => by_key
                .entry((false, name.clone(), None))
                .or_default()
                .push(row),
            Kind::Other => {}
        }
    }
    for ((texture, name, role), rows) in by_key {
        if rows.len() < 2 {
            continue;
        }
        let what = match (texture, role) {
            (true, Some(role)) => {
                format!("textures are named {name} with the role {}", role.as_str())
            }
            (true, None) => format!("textures are named {name} in the same layout"),
            (false, _) => format!("models or animations are named {name}"),
        };
        let paths: Vec<&str> = rows.iter().map(|r| r.asset.file_path.as_str()).collect();
        problems.push(Problem {
            message: format!(
                "{} {what}: {}. Rename one so each name is unique",
                rows.len(),
                paths.join(" and ")
            ),
            assets: rows.iter().map(|r| r.asset.clone()).collect(),
        });
    }
}

/// The manifest of the assets no problem names, and the problems its schema validation and the
/// importer's rules (C1) find, each naming the asset at its pointer.
fn manifest(
    domain: &str,
    reads: &[Read],
    export_packs: &[packs::ExportPack],
    problems: &[Problem],
) -> (Value, Vec<Problem>) {
    let refused: HashSet<&str> = problems
        .iter()
        .flat_map(|p| &p.assets)
        .map(|a| a.asset_id.as_str())
        .collect();
    let mut entries: Vec<(Value, &AssetRef)> = Vec::new();
    let mut used_packs = std::collections::BTreeSet::new();
    for read in reads {
        let (Some(texture), Some(pack)) = (&read.ready, read.pack) else {
            continue;
        };
        if refused.contains(read.row.asset.asset_id.as_str()) {
            continue;
        }
        used_packs.insert(pack);
        let role = texture.role.as_str();
        entries.push((
            json!({
                "kind": "texture",
                "assetName": texture.name,
                "file": format!("Textures/{}_{role}.{}", texture.name, texture.extension),
                "sha256": texture.sha256,
                "pack": export_packs[pack].id,
                "sourcePath": texture.source_path,
                "conversion": [],
                "role": role,
            }),
            &read.row.asset,
        ));
    }
    let kind_rank = |v: &Value| match v["kind"].as_str() {
        Some("model") => 0,
        Some("texture") => 1,
        _ => 2,
    };
    entries.sort_by(|(a, _), (b, _)| {
        (kind_rank(a), a["assetName"].as_str(), a["role"].as_str()).cmp(&(
            kind_rank(b),
            b["assetName"].as_str(),
            b["role"].as_str(),
        ))
    });
    let mut packs: Vec<&packs::ExportPack> = used_packs.iter().map(|p| &export_packs[*p]).collect();
    packs.sort_by(|a, b| a.id.cmp(&b.id));
    let manifest = json!({
        "contractVersion": contract::CONTRACT_VERSION,
        "domain": domain,
        "exportedAt": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        "exporter": { "name": "OpenDAM", "version": env!("CARGO_PKG_VERSION") },
        "packs": packs.iter().map(|p| p.manifest_entry()).collect::<Vec<_>>(),
        "assets": entries.iter().map(|(v, _)| v.clone()).collect::<Vec<_>>(),
    });

    let mut found = Vec::new();
    let errors = [
        contract::validate_manifest(&manifest),
        contract::check_rules(&manifest),
    ];
    // A domain name that isn't PascalCase is already a problem of its own
    let reported = |e: &String| !is_pascal(domain) && e.starts_with("/domain: ");
    for error in errors
        .into_iter()
        .filter_map(Result::err)
        .flatten()
        .filter(|e| !reported(e))
    {
        // `/assets/<i>...: message` names the asset at that index
        let asset = error
            .strip_prefix("/assets/")
            .and_then(|rest| rest.split(['/', ':']).next()?.parse::<usize>().ok())
            .and_then(|i| entries.get(i))
            .map(|(_, asset)| (*asset).clone());
        found.push(Problem {
            message: match &asset {
                Some(asset) => format!("{}: the manifest is invalid at {error}", asset.file_path),
                None => format!("The manifest is invalid at {error}"),
            },
            assets: asset.into_iter().collect(),
        });
    }
    (manifest, found)
}

/// `assets` with the status their problems give them.
fn with_status(mut assets: Vec<CheckedAsset>, problems: &[Problem]) -> Vec<CheckedAsset> {
    let refused: HashSet<&str> = problems
        .iter()
        .flat_map(|p| &p.assets)
        .map(|a| a.asset_id.as_str())
        .collect();
    for asset in &mut assets {
        if refused.contains(asset.asset.asset_id.as_str()) {
            asset.status = Status::Problem;
        }
    }
    assets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{insert_collection, memory_database, memory_library_pool};
    use serde_json::json;
    use std::path::PathBuf;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/game-export/library"
    );

    /// The fixture library copied into a temporary folder and indexed, an export root outside
    /// it, and the root collection `DungeonGame`, both set.
    struct Fixture {
        db: Database,
        library: LibraryHandle,
        export_root: PathBuf,
        dir: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("library");
        for entry in walkdir::WalkDir::new(FIXTURE) {
            let entry = entry.unwrap();
            let target = root.join(entry.path().strip_prefix(FIXTURE).unwrap());
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&target).unwrap();
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
        let library = LibraryHandle {
            id: "fixture".into(),
            name: "Fixture".into(),
            root_path: root.clone(),
            pool: memory_library_pool().await,
        };
        let db = memory_database(vec![library.clone()]).await;
        sqlx::query(
            "INSERT INTO libraries (id, name, root_paths, created_at) VALUES ('fixture', 'Fixture', ?, 0)",
        )
        .bind(json!([root]).to_string())
        .execute(&db.master_pool)
        .await
        .unwrap();
        crate::watcher::index_paths(&db, "fixture", &[root.display().to_string()])
            .await
            .unwrap();

        let export_root = dir.path().join("export");
        std::fs::create_dir(&export_root).unwrap();
        insert_collection(&db.master_pool, "dg", "DungeonGame", None).await;
        settings::update(
            &db.master_pool,
            json!({"root": export_root, "rootCollection": "dg"}),
        )
        .await
        .unwrap();
        Fixture {
            db,
            library,
            export_root,
            dir,
        }
    }

    /// Opens, registers and indexes a second library `id` at `root`, beside the fixture.
    async fn add_library(f: &Fixture, id: &str, root: &Path) -> LibraryHandle {
        let library = LibraryHandle {
            id: id.into(),
            name: id.into(),
            root_path: root.to_path_buf(),
            pool: memory_library_pool().await,
        };
        f.db.libraries
            .write()
            .await
            .insert(id.into(), library.clone());
        sqlx::query("INSERT INTO libraries (id, name, root_paths, created_at) VALUES (?, ?, ?, 0)")
            .bind(id)
            .bind(id)
            .bind(json!([root]).to_string())
            .execute(&f.db.master_pool)
            .await
            .unwrap();
        crate::watcher::index_paths(&f.db, id, &[root.display().to_string()])
            .await
            .unwrap();
        library
    }

    async fn id_of(pool: &Pool, path: &str) -> String {
        sqlx::query_scalar("SELECT id FROM assets WHERE file_path = ?")
            .bind(path)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|e| panic!("{path} isn't indexed: {e}"))
    }

    type Pool = sqlx::Pool<sqlx::Sqlite>;

    /// Adds collection `id` named `name` under `parent`, holding the fixture's files at `paths`.
    async fn collection(f: &Fixture, id: &str, name: &str, parent: &str, paths: &[&str]) {
        insert_collection(&f.db.master_pool, id, name, Some(parent)).await;
        add_members(&f.library, id, paths).await;
    }

    /// Puts the files at `paths` of `library` in collection `id`.
    async fn add_members(library: &LibraryHandle, id: &str, paths: &[&str]) {
        for path in paths {
            sqlx::query("INSERT INTO collection_assets (collection_id, asset_id) VALUES (?, ?)")
                .bind(id)
                .bind(id_of(&library.pool, path).await)
                .execute(&library.pool)
                .await
                .unwrap();
        }
    }

    /// Checks a new domain holding `paths`.
    async fn check_domain(f: &Fixture, paths: &[&str]) -> CheckReport {
        collection(f, "domain", "Props", "dg", paths).await;
        check(&f.db, "domain").await.unwrap()
    }

    fn messages(report: &CheckReport) -> Vec<&str> {
        report.problems.iter().map(|p| p.message.as_str()).collect()
    }

    /// Every manifest a check builds passes the schema and the importer's rules.
    fn assert_valid(report: &CheckReport) {
        assert_eq!(contract::validate_manifest(&report.manifest), Ok(()));
        assert_eq!(contract::check_rules(&report.manifest), Ok(()));
    }

    fn texture_entries(report: &CheckReport) -> Vec<(String, String, String)> {
        report.manifest["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                let text = |k: &str| a[k].as_str().unwrap().to_string();
                (text("assetName"), text("role"), text("file"))
            })
            .collect()
    }

    #[tokio::test]
    async fn pass_through_textures_export_with_default_names_packs_and_hashes() {
        let f = fixture().await;
        let report = check_domain(
            &f,
            &[
                "Dungeon Props/Textures/T_StoneFloor_D.png",
                "Dungeon Props/Textures/T_StoneFloor_N.png",
                "Dungeon Props/Textures/T_Rune_AO.png",
                "Dungeon Props/Textures/mattress_01.png",
                "Kenney Crates/crate_wood.png",
                "Unity Moss/T_Moss_D.png",
            ],
        )
        .await;

        assert_eq!(messages(&report), Vec::<&str>::new());
        assert_valid(&report);
        assert_eq!(report.domain, "Props");
        assert_eq!(
            report.bundle_path.as_deref(),
            Some(f.export_root.join("DungeonGame/Props").to_str().unwrap())
        );
        assert!(report.assets.iter().all(|a| a.status == Status::Ready));
        assert_eq!(
            texture_entries(&report),
            [
                ("CrateWood", "BaseColor", "Textures/CrateWood_BaseColor.png"),
                (
                    "Mattress01",
                    "BaseColor",
                    "Textures/Mattress01_BaseColor.png"
                ),
                ("Moss", "BaseColor", "Textures/Moss_BaseColor.png"),
                ("Rune", "Occlusion", "Textures/Rune_Occlusion.png"),
                (
                    "StoneFloor",
                    "BaseColor",
                    "Textures/StoneFloor_BaseColor.png"
                ),
                ("StoneFloor", "Normal", "Textures/StoneFloor_Normal.png"),
            ]
            .map(|(n, r, p)| (n.to_string(), r.to_string(), p.to_string()))
        );

        let assets = report.manifest["assets"].as_array().unwrap();
        let floor = &assets[4];
        let source = f
            .library
            .root_path
            .join("Dungeon Props/Textures/T_StoneFloor_D.png");
        assert_eq!(floor["sha256"], hashes::sha256_file(&source).unwrap());
        assert_eq!(floor["sourcePath"], "Textures/T_StoneFloor_D.png");
        assert_eq!(floor["pack"], "dungeon-props");
        assert_eq!(floor["conversion"], json!([]));
        assert_eq!(assets[0]["sourcePath"], "crate_wood.png");

        assert_eq!(
            report.manifest["packs"],
            json!([
                {
                    "id": "dungeon-props",
                    "name": "Dungeon Props",
                    "version": "1.2.0",
                    "publisher": "Stylarts",
                    "sourceUrl": "https://example.com/dungeon-props",
                    "licence": {
                        "spdx": "LicenseRef-Stylarts-EULA",
                        "name": "Stylarts Standard EULA",
                        "url": "https://example.com/stylarts-eula"
                    },
                    "origin": "generic"
                },
                {
                    "id": "kenney-crates",
                    "name": "Crate Kit",
                    "version": "1.0",
                    "licence": { "spdx": "CC0-1.0", "name": "CC0" },
                    "origin": "generic"
                },
                {
                    "id": "unity-moss",
                    "name": "Unity Moss",
                    "licence": { "spdx": "MIT", "name": "MIT" },
                    "origin": "unity"
                }
            ])
        );

        // The hashes are memoized for the next check
        let memo: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_hashes")
            .fetch_one(&f.library.pool)
            .await
            .unwrap();
        assert_eq!(memo, 6);
        let again = check(&f.db, "domain").await.unwrap();
        assert_eq!(again.manifest["assets"], report.manifest["assets"]);
    }

    #[tokio::test]
    async fn clashing_pack_ids_take_free_suffixes_and_an_empty_slug_is_pack() {
        let f = fixture().await;
        let report = check_domain(
            &f,
            &[
                "Foo A/tile_a.png",
                "Foo B/tile_b.png",
                "Foo Two/tile_two.png",
                "---/dash.png",
            ],
        )
        .await;

        assert_eq!(messages(&report), Vec::<&str>::new());
        assert_valid(&report);
        let ids: Vec<(&str, &str)> = report
            .packs
            .iter()
            .map(|p| (p.path.as_str(), p.pack.id.as_str()))
            .collect();
        assert_eq!(
            ids,
            [
                ("---", "pack"),
                ("Foo A", "foo"),
                ("Foo B", "foo-3"),
                ("Foo Two", "foo-2")
            ]
        );
        let dash = &report.packs[0].pack;
        assert_eq!(
            json!(dash.licence),
            json!({ "spdx": null, "name": "Unknown" })
        );
        let packs: Vec<&str> = report.manifest["packs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        assert_eq!(packs, ["foo", "foo-2", "foo-3", "pack"]);
    }

    #[tokio::test]
    async fn guards_refuse_conversions_and_fbx_naming_the_asset() {
        let f = fixture().await;
        let report = check_domain(
            &f,
            &[
                "Converted/Ruins/T_Pillar_D.png",
                "Converted/Ruins/T_Pillar_N.png",
                "Dungeon Props/Textures/T_StoneFloor_ORM.png",
                "Dungeon Props/Textures/T_Wall_N_DX.png",
                "Dungeon Props/Textures/T_Wall_D.jpg",
                "Dungeon Props/Models/SM_Chest.fbx",
            ],
        )
        .await;

        assert_eq!(
            messages(&report),
            [
                "Converted/Ruins/T_Pillar_D.png: conversion.json support not built yet (Converted/conversion.json covers this file)",
                "Converted/Ruins/T_Pillar_N.png: conversion.json support not built yet (Converted/conversion.json covers this file)",
                "Converted/Ruins/T_Pillar_N.png: texture conversion not built yet: it needs its DirectX normal map's green channel flipped",
                "Dungeon Props/Models/SM_Chest.fbx: model export not built yet (an FBX exports as a model or an animation, and neither is built yet)",
                "Dungeon Props/Textures/T_StoneFloor_ORM.png: texture conversion not built yet: it needs its ORM channels split into URP roles",
                "Dungeon Props/Textures/T_Wall_D.jpg: texture conversion not built yet: it needs its .jpg format to PNG",
                "Dungeon Props/Textures/T_Wall_N_DX.png: texture conversion not built yet: it needs its DirectX normal map's green channel flipped",
            ]
        );
        for problem in &report.problems {
            let path = problem.message.split(':').next().unwrap();
            assert_eq!(problem.assets.len(), 1);
            assert_eq!(problem.assets[0].file_path, path);
        }
        assert!(report.assets.iter().all(|a| a.status == Status::Problem));
        assert_eq!(report.manifest["assets"], json!([]));
        assert_valid(&report);
        let ruins = report.packs.iter().find(|p| p.pack.id == "ruins").unwrap();
        assert_eq!(ruins.pack.origin, packs::Origin::Unreal);
        assert_eq!(ruins.normal_convention, Convention::DirectX);
        assert_eq!(ruins.pack.licence.spdx.as_deref(), Some("CC-BY-4.0"));
    }

    #[tokio::test]
    async fn a_group_member_reads_as_its_main_item_and_members_count_once() {
        let f = fixture().await;
        let obj = "Dungeon Props/Models/SM_Chest.obj";
        let floor = "Dungeon Props/Textures/T_StoneFloor_D.png";
        let group: Option<String> =
            sqlx::query_scalar("SELECT group_id FROM assets WHERE file_path = ?")
                .bind(obj)
                .fetch_one(&f.library.pool)
                .await
                .unwrap();
        let fbx = id_of(&f.library.pool, "Dungeon Props/Models/SM_Chest.fbx").await;
        assert_eq!(
            group.as_deref(),
            Some(fbx.as_str()),
            "the FBX is the main item"
        );

        collection(&f, "domain", "Props", "dg", &[obj, floor]).await;
        collection(&f, "sub", "Floors", "domain", &[floor]).await;
        collection(&f, "subsub", "Stone", "sub", &[floor]).await;
        let report = check(&f.db, "domain").await.unwrap();

        let rows: Vec<(&str, Kind, Option<&str>)> = report
            .assets
            .iter()
            .map(|a| (a.asset.file_path.as_str(), a.kind, a.asset_name.as_deref()))
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "Dungeon Props/Models/SM_Chest.fbx",
                    Kind::Model,
                    Some("Chest")
                ),
                (floor, Kind::Texture, Some("StoneFloor")),
            ]
        );
        assert_eq!(report.assets[0].asset.asset_id, fbx);
    }

    #[tokio::test]
    async fn members_that_cant_export_are_refused_by_name() {
        let f = fixture().await;
        let report = check_domain(
            &f,
            &[
                "Dungeon Props/Models/SM_Barrel.obj",
                "Dungeon Props/Docs/notes.md",
                "Dungeon Props/Textures/T_Sky.exr",
                "Loose/stray.png",
            ],
        )
        .await;

        assert_eq!(
            messages(&report),
            [
                "Dungeon Props/Docs/notes.md: it is neither a model, an animation nor an image, so it can't be exported",
                "Dungeon Props/Models/SM_Barrel.obj: it has no FBX file: a model exports from its group's FBX",
                "Dungeon Props/Textures/T_Sky.exr: a .exr texture can't be exported: use png, tga, psd or tif",
                "Loose/stray.png: it is in no pack: mark its folder as a pack",
            ]
        );
        let kinds: Vec<Kind> = report.assets.iter().map(|a| a.kind).collect();
        assert_eq!(
            kinds,
            [Kind::Model, Kind::Texture, Kind::Texture, Kind::Other]
        );
        let stray = report
            .assets
            .iter()
            .find(|a| a.asset.file_path == "Loose/stray.png")
            .unwrap();
        assert_eq!(stray.pack, None);
        assert_eq!(stray.asset_name.as_deref(), Some("Stray"));
        assert_valid(&report);
    }

    #[tokio::test]
    async fn duplicate_names_are_one_problem_naming_both_assets() {
        let f = fixture().await;
        let report = check_domain(
            &f,
            &[
                "Dungeon Props/Textures/T_Banner_BaseColor.png",
                "Kenney Crates/banner.png",
                "Kenney Crates/crate_wood.png",
            ],
        )
        .await;

        assert_eq!(
            messages(&report),
            ["2 textures are named Banner with the role BaseColor: Dungeon Props/Textures/T_Banner_BaseColor.png and Kenney Crates/banner.png. Rename one so each name is unique"]
        );
        let named: Vec<&str> = report.problems[0]
            .assets
            .iter()
            .map(|a| a.file_path.as_str())
            .collect();
        assert_eq!(
            named,
            [
                "Dungeon Props/Textures/T_Banner_BaseColor.png",
                "Kenney Crates/banner.png"
            ]
        );
        assert_eq!(
            texture_entries(&report),
            [(
                "CrateWood".to_string(),
                "BaseColor".to_string(),
                "Textures/CrateWood_BaseColor.png".to_string()
            )]
        );
        assert_valid(&report);
    }

    #[tokio::test]
    async fn a_domain_must_be_a_pascal_case_child_of_the_root_with_no_smart_collection() {
        let f = fixture().await;
        collection(&f, "lower", "props", "dg", &[]).await;
        collection(&f, "smart", "Recent", "lower", &[]).await;
        sqlx::query("UPDATE collections SET is_smart = 1 WHERE id = 'smart'")
            .execute(&f.db.master_pool)
            .await
            .unwrap();
        collection(&f, "nested", "Nested", "lower", &[]).await;

        let report = check(&f.db, "lower").await.unwrap();
        assert_eq!(
            messages(&report),
            [
                "The domain's name props isn't PascalCase (a capital letter, then letters and digits): it is a folder of the bundle path and the manifest's domain",
                "Recent is a smart collection: a domain and its subcollections must be ordinary collections",
            ]
        );

        let report = check(&f.db, "nested").await.unwrap();
        assert_eq!(
            messages(&report),
            ["Nested is not a domain: domains are the collections directly inside the root collection DungeonGame"]
        );

        sqlx::query("UPDATE collections SET name = 'Dungeon Game' WHERE id = 'dg'")
            .execute(&f.db.master_pool)
            .await
            .unwrap();
        collection(&f, "props", "Props", "dg", &[]).await;
        let report = check(&f.db, "props").await.unwrap();
        assert_eq!(
            messages(&report),
            ["The root collection's name Dungeon Game isn't PascalCase (a capital letter, then letters and digits): it is a folder of the bundle path"]
        );

        assert!(check(&f.db, "missing").await.is_err());
    }

    #[tokio::test]
    async fn unset_settings_and_a_bad_export_root_are_problems() {
        let f = fixture().await;
        collection(
            &f,
            "props",
            "Props",
            "dg",
            &["Kenney Crates/crate_wood.png"],
        )
        .await;
        settings::update(
            &f.db.master_pool,
            json!({"root": null, "rootCollection": null}),
        )
        .await
        .unwrap();

        let report = check(&f.db, "props").await.unwrap();
        assert_eq!(
            messages(&report),
            [
                "Set the export root in Settings, under Game export",
                "Set the root collection in Settings, under Game export",
            ]
        );
        assert_eq!(report.bundle_path, None);

        // An export root saved before a library moved around it is refused at the check (C2)
        let inside = f.library.root_path.join("Kenney Crates");
        sqlx::query("INSERT INTO settings (key, value) VALUES ('export.root', ?), ('export.rootCollection', '\"dg\"')")
            .bind(json!(inside).to_string())
            .execute(&f.db.master_pool)
            .await
            .unwrap();
        let report = check(&f.db, "props").await.unwrap();
        assert_eq!(report.problems.len(), 1, "{:?}", messages(&report));
        assert!(
            report.problems[0]
                .message
                .contains("is inside the library Fixture"),
            "{:?}",
            messages(&report)
        );
    }

    #[tokio::test]
    async fn a_domain_spanning_two_libraries_reads_each_with_its_own_packs_and_files() {
        let f = fixture().await;
        // A second library: a pack also named `foo`, and a pack under its own conversion.json
        let root = f.dir.path().join("second");
        let write = |path: &str, bytes: &[u8]| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        };
        let png = std::fs::read(Path::new(FIXTURE).join("Foo B/tile_b.png")).unwrap();
        write(
            "Foo Copy/datapackage.json",
            br#"{"name": "foo", "title": "Foo Copy"}"#,
        );
        write("Foo Copy/tile_c.png", &png);
        write(
            "Unreal/conversion.json",
            br#"{"contractVersion": 1, "steps": [{"tool": "UE", "toolVersion": "5.4", "from": "uasset"}]}"#,
        );
        write("Unreal/Rocks/datapackage.json", br#"{"name": "rocks"}"#);
        write("Unreal/Rocks/T_Rock_D.png", &png);
        let second = add_library(&f, "second", &root).await;

        collection(&f, "domain", "Props", "dg", &["Foo A/tile_a.png"]).await;
        add_members(
            &second,
            "domain",
            &["Foo Copy/tile_c.png", "Unreal/Rocks/T_Rock_D.png"],
        )
        .await;
        let report = check(&f.db, "domain").await.unwrap();

        assert_eq!(
            messages(&report),
            ["Unreal/Rocks/T_Rock_D.png: conversion.json support not built yet (Unreal/conversion.json covers this file)"]
        );
        assert_eq!(report.problems[0].assets[0].library_id, "second");
        let rows: Vec<(&str, &str, Option<&str>, Status)> = report
            .assets
            .iter()
            .map(|a| {
                let library = a.asset.library_id.as_str();
                (
                    library,
                    a.asset.file_path.as_str(),
                    a.pack.as_deref(),
                    a.status,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "second",
                    "Unreal/Rocks/T_Rock_D.png",
                    Some("rocks"),
                    Status::Problem
                ),
                ("fixture", "Foo A/tile_a.png", Some("foo"), Status::Ready),
                (
                    "second",
                    "Foo Copy/tile_c.png",
                    Some("foo-2"),
                    Status::Ready
                ),
            ]
        );
        // Packs in library order, then path, each origin read in its own library
        let packs: Vec<(&str, &str, &str, packs::Origin)> = report
            .packs
            .iter()
            .map(|p| {
                let pack = &p.pack;
                (
                    p.library_id.as_str(),
                    p.path.as_str(),
                    pack.id.as_str(),
                    pack.origin,
                )
            })
            .collect();
        assert_eq!(
            packs,
            [
                ("fixture", "Foo A", "foo", packs::Origin::Generic),
                ("second", "Foo Copy", "foo-2", packs::Origin::Generic),
                ("second", "Unreal/Rocks", "rocks", packs::Origin::Unreal),
            ]
        );

        assert_valid(&report);
        let manifest_packs: Vec<&str> = report.manifest["packs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        assert_eq!(manifest_packs, ["foo", "foo-2"]);
        let assets = report.manifest["assets"].as_array().unwrap();
        let entries: Vec<(&str, &str, &str)> = assets
            .iter()
            .map(|a| {
                let text = |k: &str| a[k].as_str().unwrap();
                (text("assetName"), text("pack"), text("sourcePath"))
            })
            .collect();
        assert_eq!(
            entries,
            [
                ("TileA", "foo", "tile_a.png"),
                ("TileC", "foo-2", "tile_c.png")
            ]
        );
        assert_eq!(
            assets[1]["sha256"],
            hashes::sha256_file(&root.join("Foo Copy/tile_c.png")).unwrap()
        );
        // Each library memoizes its own file's hash
        for library in [&f.library, &second] {
            let memo: Vec<String> = sqlx::query_scalar("SELECT file_path FROM file_hashes")
                .fetch_all(&library.pool)
                .await
                .unwrap();
            let expected = if library.id == "second" {
                "Foo Copy/tile_c.png"
            } else {
                "Foo A/tile_a.png"
            };
            assert_eq!(memo, [expected], "{}", library.id);
        }
    }

    #[tokio::test]
    async fn a_file_name_that_gives_no_pascal_case_name_is_a_problem() {
        let f = fixture().await;
        let report = check_domain(
            &f,
            &["Kenney Crates/___.png", "Kenney Crates/crate_wood.png"],
        )
        .await;

        assert_eq!(
            messages(&report),
            ["Kenney Crates/___.png: its file name gives no PascalCase name (a capital letter, then letters and digits)"]
        );
        assert_eq!(
            report.problems[0].assets[0].file_path,
            "Kenney Crates/___.png"
        );
        let blank = report
            .assets
            .iter()
            .find(|a| a.asset.file_path == "Kenney Crates/___.png")
            .unwrap();
        assert_eq!(
            (blank.asset_name.as_deref(), blank.status),
            (Some(""), Status::Problem)
        );
        assert_eq!(
            texture_entries(&report),
            [(
                "CrateWood".to_string(),
                "BaseColor".to_string(),
                "Textures/CrateWood_BaseColor.png".to_string()
            )]
        );
        assert_valid(&report);
    }

    #[tokio::test]
    async fn a_deleted_root_collection_is_a_problem() {
        let f = fixture().await;
        collection(
            &f,
            "props",
            "Props",
            "dg",
            &["Kenney Crates/crate_wood.png"],
        )
        .await;
        sqlx::query("DELETE FROM collections WHERE id = 'dg'")
            .execute(&f.db.master_pool)
            .await
            .unwrap();

        let report = check(&f.db, "props").await.unwrap();
        assert_eq!(
            messages(&report),
            ["The root collection dg set in Settings no longer exists"]
        );
        assert_eq!(report.bundle_path, None);
    }
}
