//! The Export view's Check (plan game-export C14): reads a domain, builds its manifest in memory,
//! validates it, and lists every asset with its kind, default `assetName`, pack and status, and
//! every problem that would refuse the export.
//!
//! A domain is a direct child of the root collection (C3). Its assets are the grid items in it
//! and all its subcollections, across every library, each once. A format group with an FBX, or
//! a lone FBX, is a model or an animation by its FBX's facts (`fbx_facts`). Until later steps of
//! the plan's build order land, only standalone textures that pass through byte for byte export,
//! each with its nearest `conversion.json`'s steps (C10): the guards refuse a texture that needs
//! converting, every model and every animation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;
use serde_json::{json, Value};

use crate::db::{Database, LibraryHandle};
use crate::export::conversion::{Invalid, Walker};
use crate::export::fbx_facts::{self, FbxFacts, Reading};
use crate::export::infer::{self, PackImages, UnityPack};
use crate::export::names::{is_pascal, pascal};
use crate::export::packs::{self, Origin, PackSource};
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
    /// Whether it is in the domain itself, rather than joined through a model's material or
    /// its texture set (C3)
    pub member: bool,
    /// A texture's layout (C8)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<&'static str>,
    /// A normal map's convention (D14)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normal_convention: Option<Convention>,
    /// A model's materials (C7)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub materials: Vec<infer::Material>,
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

/// A pack of the domain: its manifest entry, where it is, and its normal-map convention, yours
/// else its origin's default.
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

/// The FBX a model or animation exports from (A5), and its facts.
struct ItemFbx {
    /// Relative to the library root
    path: String,
    facts: Result<FbxFacts, String>,
}

/// What a check reads from one library before it touches a file other than an FBX.
struct LibraryInput {
    library: LibraryHandle,
    items: Vec<ItemRow>,
    /// The FBX of each item that is a format group with an FBX member, or a lone FBX, by id
    fbx: HashMap<String, ItemFbx>,
    /// The pack at or above each item, by its index in `items`
    item_packs: Vec<Option<usize>>,
    packs: Vec<PackFolder>,
    /// The indexed files of each pack an item is in, by its index in `packs`
    pack_files: HashMap<usize, Vec<String>>,
    /// The indexed images among them
    pack_images: HashMap<usize, Vec<infer::Image>>,
    memo: hashes::Memo,
}

/// One message per general problem, before any asset is read.
fn general(message: impl Into<String>) -> Problem {
    Problem {
        message: message.into(),
        assets: Vec::new(),
    }
}

/// Checks the domain collection `domain_id`, reading FBX files' facts with `facts`. An unknown
/// collection is an error; everything else that would refuse the export is a problem in the
/// report.
pub async fn check(
    db: &Database,
    domain_id: &str,
    facts: &impl fbx_facts::Reader,
) -> Result<CheckReport, String> {
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
            read_library(library, &subtree, facts)
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

/// An indexed file of a pack: its id, path and kind.
type PackFile = (String, String, String);

/// The indexed files of pack `pack`, at `path` among `packs`, without the files of the packs
/// nested in it, which are their own.
async fn files_of_pack(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    packs: &HashMap<&str, usize>,
    pack: usize,
    path: &str,
) -> sqlx::Result<Vec<PackFile>> {
    let below: Vec<PackFile> = sqlx::query_as(
        "SELECT id, file_path, kind FROM assets
         WHERE ?1 = '' OR (file_path >= ?1 || '/' AND file_path < ?1 || '0')",
    )
    .bind(path)
    .fetch_all(pool)
    .await?;
    Ok(below
        .into_iter()
        .filter(|(_, f, _)| innermost_pack(packs, dir_of(f)) == Some(pack))
        .collect())
}

/// Whether a file of kind `kind` is an image, which exports as a texture.
fn is_image(kind: &str) -> bool {
    matches!(
        AssetKind::from_str(kind),
        AssetKind::Texture | AssetKind::Image | AssetKind::Hdri
    )
}

/// The origin the pack at `path` has without your override (A1), for the pack edit form, or
/// `None` when no pack is at `path`. It reads the pack's files as a check does.
pub async fn inferred_origin(
    library: &LibraryHandle,
    path: &str,
) -> Result<Option<packs::Origin>, String> {
    let path = path.trim_matches('/');
    let pack_paths: Vec<String> = sqlx::query_scalar("SELECT path FROM folders WHERE is_pack")
        .fetch_all(&library.pool)
        .await
        .map_err(|e| e.to_string())?;
    let by_path: HashMap<&str, usize> = pack_paths
        .iter()
        .enumerate()
        .map(|(i, p)| (p.as_str(), i))
        .collect();
    let Some(&pack) = by_path.get(path) else {
        return Ok(None);
    };
    let files: Vec<String> = files_of_pack(&library.pool, &by_path, pack, path)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(_, f, _)| f)
        .collect();
    let root = library.root_path.clone();
    tokio::task::spawn_blocking(move || {
        Some(packs::infer_origin(&root, &files, &mut Walker::new(&root)))
    })
    .await
    .map_err(|e| e.to_string())
}

/// Whether `path` lies in a Unity FBX format folder, `fbx(unity)` or `fbx (unity)` in any case.
fn in_unity_fbx_folder(path: &str) -> bool {
    let mut folders = path.split('/').rev().skip(1);
    folders.any(|f| {
        matches!(
            f.to_ascii_lowercase().as_str(),
            "fbx(unity)" | "fbx (unity)"
        )
    })
}

/// The FBX a group exports from (A5): its first FBX member in a Unity FBX format folder, else
/// its first, by path ignoring case.
fn exported_fbx<'a>(paths: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    paths
        .into_iter()
        .min_by_key(|p| (!in_unity_fbx_folder(p), p.to_lowercase(), *p))
}

async fn read_library(
    library: LibraryHandle,
    subtree: &[String],
    facts: &impl fbx_facts::Reader,
) -> sqlx::Result<LibraryInput> {
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
    let group_fbx: Vec<(String, String)> = sqlx::query_as(
        "SELECT group_id, file_path FROM assets
         WHERE group_id IN (SELECT value FROM json_each(?)) AND lower(extension) = 'fbx'",
    )
    .bind(serde_json::to_string(&item_ids).expect("ids serialize"))
    .fetch_all(pool)
    .await?;
    let mut fbx_paths: HashMap<&str, Vec<&str>> = HashMap::new();
    for (group, path) in &group_fbx {
        fbx_paths.entry(group).or_default().push(path);
    }
    // A lone FBX is a group of one
    for item in &items {
        if item.extension.eq_ignore_ascii_case("fbx") {
            fbx_paths
                .entry(&item.id)
                .or_insert_with(|| vec![&item.file_path]);
        }
    }
    // Every read is sent at once, so the preview queue serves them all between two of its jobs
    let reads = fbx_paths.into_iter().map(|(id, paths)| {
        let path = exported_fbx(paths)
            .expect("a group with an FBX")
            .to_string();
        let root = &library.root_path;
        async move {
            let facts = facts.read(&root.join(&path), root).await;
            (id.to_string(), ItemFbx { path, facts })
        }
    });
    let fbx: HashMap<String, ItemFbx> = futures_util::future::join_all(reads)
        .await
        .into_iter()
        .collect();

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
    let mut pack_images = HashMap::new();
    for pack in item_packs.iter().flatten().collect::<HashSet<_>>() {
        let files = files_of_pack(pool, &by_path, *pack, &packs[*pack].path).await?;
        let images: Vec<infer::Image> = files
            .iter()
            .filter(|(_, _, kind)| is_image(kind))
            .map(|(id, path, _)| infer::Image {
                id: id.clone(),
                path: path.clone(),
            })
            .collect();
        pack_files.insert(*pack, files.into_iter().map(|(_, f, _)| f).collect());
        pack_images.insert(*pack, images);
    }

    // Textures join through materials and sets, so every image of a pack may be hashed
    let paths: Vec<String> = items
        .iter()
        .map(|i| i.file_path.clone())
        .chain(pack_images.values().flatten().map(|i| i.path.clone()))
        .collect();
    let memo = hashes::load(pool, &paths).await?;
    Ok(LibraryInput {
        fbx,
        library,
        items,
        item_packs,
        packs,
        pack_files,
        pack_images,
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
    conversion: Vec<Value>,
}

/// One asset as the check reads it, before names are compared.
struct Read {
    row: CheckedAsset,
    /// Index of its pack in the domain's packs
    pack: Option<usize>,
    ready: Option<ReadyTexture>,
}

type Evaluated = (Vec<CheckedAsset>, Vec<DomainPack>, Vec<Problem>, Value);

/// A pack of a library, by the library's and the pack's indices in `inputs`.
type PackKey = (usize, usize);

/// One image the domain exports as a texture: a member, or a source joined through a material
/// or its texture set (C3).
struct TextureJob {
    library: usize,
    asset: AssetRef,
    extension: String,
    /// Index of its pack in the domain's packs
    pack: Option<usize>,
    name: String,
    layout: Layout,
    convention: Convention,
    member: bool,
}

/// Everything a check does with files, run on a blocking thread: reads each item, its pack and
/// its file, infers the models' materials and the texture sets, compares names, and builds and
/// validates the manifest.
fn evaluate(domain: &str, inputs: &mut [LibraryInput]) -> Evaluated {
    let mut problems = Vec::new();
    let mut reads: Vec<Read> = Vec::new();
    // The domain's packs in manifest order (library order, then path), by (library, pack index)
    let mut pack_order: Vec<PackKey> = Vec::new();
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
                origin: pack
                    .edits
                    .origin
                    .unwrap_or_else(|| packs::infer_origin(root, files, &mut walkers[*l])),
                info: pack.info.clone(),
                edits: pack.edits.clone(),
            }
        })
        .collect();
    let export_packs = packs::export_packs(&sources);
    let pack_index: HashMap<PackKey, usize> = pack_order
        .iter()
        .enumerate()
        .map(|(i, key)| (*key, i))
        .collect();

    // Each pack's images in their texture sets (C8), and what a Unity pack's files say
    let mut catalogs: HashMap<PackKey, PackImages> = HashMap::new();
    for (l, input) in inputs.iter_mut().enumerate() {
        for (p, images) in input.pack_images.drain() {
            catalogs.insert((l, p), PackImages::new(images));
        }
    }
    let mut unity_packs: HashMap<PackKey, UnityPack> = HashMap::new();

    // Each model with materials, as (its index in `reads`, its pack), and each member image in a
    // pack, as (its pack, its index in the pack's images)
    let mut models: Vec<(usize, PackKey)> = Vec::new();
    let mut members: Vec<(PackKey, usize)> = Vec::new();
    let mut jobs: Vec<TextureJob> = Vec::new();
    let mut seen = HashSet::new();
    for (l, input) in inputs.iter().enumerate() {
        let root = &input.library.root_path;
        for (i, item) in input.items.iter().enumerate() {
            if !seen.insert(item.id.clone()) {
                continue;
            }
            let asset = AssetRef {
                library_id: input.library.id.clone(),
                asset_id: item.id.clone(),
                file_path: item.file_path.clone(),
            };
            let key = input.item_packs[i].map(|p| (l, p));
            let pack = key.map(|key| pack_index[&key]);
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
            let mut row = CheckedAsset {
                asset: asset.clone(),
                kind: Kind::Other,
                asset_name: None,
                role: None,
                pack: pack.map(|p| export_packs[p].id.clone()),
                status: Status::Ready,
                member: true,
                layout: None,
                normal_convention: None,
                materials: Vec::new(),
            };
            match AssetKind::from_str(&item.kind) {
                AssetKind::Model3d => {
                    row.kind = Kind::Model;
                    row.asset_name = Some(pascal(stem));
                    match input.fbx.get(&item.id) {
                        None => {
                            issue("it has no FBX file: a model exports from its group's FBX".into())
                        }
                        Some(fbx) => {
                            row.kind = read_fbx(fbx, &item.file_path, &mut issue);
                            if let (Ok(facts), Some(key), Some(p)) = (&fbx.facts, key, pack) {
                                if facts.reading() == Some(Reading::Model) {
                                    let unity = (sources[p].origin == Origin::Unity).then(|| {
                                        &*unity_packs.entry(key).or_insert_with(|| {
                                            UnityPack::read(root, &input.pack_files[&key.1])
                                        })
                                    });
                                    let model = infer::Model {
                                        item_path: &item.file_path,
                                        fbx_path: &fbx.path,
                                        facts,
                                    };
                                    let images = catalogs
                                        .entry(key)
                                        .or_insert_with(|| PackImages::new(Vec::new()));
                                    row.materials = infer::materials(images, root, unity, &model);
                                    material_names(&row.materials, &mut issue);
                                    models.push((reads.len(), key));
                                }
                            }
                        }
                    }
                }
                AssetKind::Texture | AssetKind::Image | AssetKind::Hdri => {
                    let found = key.and_then(|key| {
                        let image = catalogs.get(&key)?.find(&item.file_path)?;
                        Some((key, image))
                    });
                    match found {
                        Some(member) => members.push(member),
                        // An image in no pack has no set: it is a texture on its own
                        None => {
                            let stem_info = textures::read_stem(stem);
                            jobs.push(TextureJob {
                                library: l,
                                asset,
                                extension: item.extension.clone(),
                                pack,
                                name: pascal(&stem_info.base),
                                layout: stem_info.layout.unwrap_or(Layout::Role(Role::BaseColor)),
                                convention: stem_info.convention.unwrap_or(Convention::OpenGl),
                                member: true,
                            });
                        }
                    }
                    continue;
                }
                _ => issue(
                    "it is neither a model, an animation nor an image, so it can't be exported"
                        .into(),
                ),
            }
            reads.push(Read {
                row,
                pack,
                ready: None,
            });
        }
    }

    // The sets the models use, with the models using them (D24)
    let mut users: BTreeMap<(PackKey, usize), Vec<infer::SetUse>> = BTreeMap::new();
    for (m, (r, key)) in models.iter().enumerate() {
        for material in &reads[*r].row.materials {
            let mut roles: BTreeMap<usize, Vec<Role>> = BTreeMap::new();
            for (role, binding) in &material.textures {
                roles.entry(binding.set).or_default().push(*role);
            }
            for (set, roles) in roles {
                users
                    .entry((*key, set))
                    .or_default()
                    .push((m, material.name.clone(), roles));
            }
        }
    }
    // A member image brings in its whole set, and a material the sources of the roles it uses
    let mut wanted: Vec<(PackKey, usize)> = Vec::new();
    for (key, image) in &members {
        let images = &catalogs[key];
        wanted.extend(
            images.sets[images.set_of[*image]]
                .sources
                .iter()
                .map(|i| (*key, *i)),
        );
    }
    // Each binding, as (model's index in `reads`, material, role, pack, set, files)
    let mut bound: Vec<(usize, usize, Role, PackKey, usize, Vec<String>)> = Vec::new();
    for (r, key) in &models {
        let images = &catalogs[key];
        for (m, material) in reads[*r].row.materials.iter().enumerate() {
            for (role, binding) in &material.textures {
                let sources = images.role_sources(binding.set, *role);
                let files = sources
                    .iter()
                    .map(|i| images.images[*i].path.clone())
                    .collect();
                wanted.extend(sources.into_iter().map(|i| (*key, i)));
                bound.push((*r, m, *role, *key, binding.set, files));
            }
        }
    }

    // Every set's default name (D24), before the rows change
    let set_names: HashMap<(PackKey, usize), String> = {
        let model_names: Vec<String> = models
            .iter()
            .map(|(r, _)| reads[*r].row.asset_name.clone().unwrap_or_default())
            .collect();
        let model_sets = |m: usize, role: Role| {
            let materials = &reads[models[m].0].row.materials;
            let sets: HashSet<usize> = materials
                .iter()
                .filter_map(|material| material.textures.get(&role).map(|b| b.set))
                .collect();
            sets.len()
        };
        wanted
            .iter()
            .map(|(key, image)| (*key, catalogs[key].set_of[*image]))
            .chain(bound.iter().map(|b| (b.3, b.4)))
            .map(|(key, set)| {
                let users = users.get(&(key, set)).map_or(&[][..], Vec::as_slice);
                let name = infer::set_name(&catalogs[&key], set, users, &model_names, model_sets);
                ((key, set), name)
            })
            .collect()
    };
    for (r, m, role, key, set, files) in bound {
        let binding = reads[r].row.materials[m]
            .textures
            .get_mut(&role)
            .expect("a bound role");
        binding.asset_name = set_names[&(key, set)].clone();
        binding.files = files;
    }
    let member_images: HashSet<(PackKey, usize)> = members.into_iter().collect();
    let mut joined = HashSet::new();
    for (key, image) in wanted {
        if !joined.insert((key, image)) {
            continue;
        }
        let (l, _) = key;
        let images = &catalogs[&key];
        let pack = pack_index[&key];
        let file = &images.images[image];
        jobs.push(TextureJob {
            library: l,
            asset: AssetRef {
                library_id: inputs[l].library.id.clone(),
                asset_id: file.id.clone(),
                file_path: file.path.clone(),
            },
            extension: file
                .path
                .rsplit_once('.')
                .map_or("", |(_, e)| e)
                .to_string(),
            pack: Some(pack),
            name: set_names[&(key, images.set_of[image])].clone(),
            layout: images.layout(image),
            convention: images.convention(image, sources[pack].normal_convention()),
            member: member_images.contains(&(key, image)),
        });
    }

    // Each invalid conversion.json, by library and path, with the assets it covers
    let mut invalid_conversions: BTreeMap<(usize, String), (Invalid, Vec<AssetRef>)> =
        BTreeMap::new();
    for job in jobs {
        let input = &mut inputs[job.library];
        let mut issue = |message: String| {
            problems.push(Problem {
                message: format!("{}: {message}", job.asset.file_path),
                assets: vec![job.asset.clone()],
            })
        };
        let mut row = CheckedAsset {
            asset: job.asset.clone(),
            kind: Kind::Texture,
            asset_name: Some(job.name.clone()),
            role: None,
            pack: job.pack.map(|p| export_packs[p].id.clone()),
            status: Status::Ready,
            member: job.member,
            layout: Some(job.layout.label()),
            normal_convention: None,
            materials: Vec::new(),
        };
        if let Layout::Role(role) = job.layout {
            row.role = Some(role);
            if role == Role::Normal {
                row.normal_convention = Some(job.convention);
            }
        }
        // A pass-through texture has no step of OpenDAM's own
        let path = &job.asset.file_path;
        let conversion = match walkers[job.library].chain(&[path], Vec::new()) {
            Ok(chain) => Some(chain),
            Err(invalid) => {
                invalid_conversions
                    .entry((job.library, invalid.path.clone()))
                    .or_insert_with(|| (invalid, Vec::new()))
                    .1
                    .push(job.asset.clone());
                None
            }
        };
        let mut ready = None;
        match (
            textures::pass_through(&job.extension, job.layout, job.convention),
            job.pack,
            conversion,
        ) {
            (Err(Refusal::NeedsConversion(m) | Refusal::Unsupported(m)), _, _) => issue(m),
            (Ok(pass), Some(pack), Some(conversion)) if is_pascal(&job.name) => {
                match input.memo.sha256(&input.library.root_path, path) {
                    Ok(sha256) => {
                        ready = Some(ReadyTexture {
                            name: job.name.clone(),
                            role: pass.role,
                            extension: pass.extension,
                            sha256,
                            source_path: relative_to(&sources[pack].path, path),
                            conversion,
                        })
                    }
                    Err(e) => issue(format!("it can't be read: {e}")),
                }
            }
            _ => {}
        }
        reads.push(Read {
            row,
            pack: job.pack,
            ready,
        });
    }
    // Each asset's problems together, in library order, then by path, as the items are
    let library_order: HashMap<&str, usize> = inputs
        .iter()
        .enumerate()
        .map(|(l, i)| (i.library.id.as_str(), l))
        .collect();
    problems.sort_by_key(|p| {
        p.assets
            .first()
            .map(|a| (library_order[a.library_id.as_str()], a.file_path.clone()))
    });
    for ((_, path), (invalid, assets)) in invalid_conversions {
        problems.push(Problem {
            message: format!(
                "{path}: this conversion.json is invalid, so no file it covers can be exported: {}",
                invalid.errors.join("; ")
            ),
            assets,
        });
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
            normal_convention: source.normal_convention(),
            library_id: source.library_id.clone(),
            path: source.path.clone(),
            pack,
        })
        .collect();
    (rows, domain_packs, problems, manifest)
}

/// The problems of a model's material names (D22, C5): a slot whose name gives no PascalCase
/// name, and two slots whose names give one name.
fn material_names(materials: &[infer::Material], issue: &mut impl FnMut(String)) {
    let mut by_name: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for material in materials {
        if is_pascal(&material.name) {
            by_name
                .entry(&material.name)
                .or_default()
                .push(&material.slot);
        } else {
            issue(format!(
                "its material slot {} gives no PascalCase name (a capital letter, then letters and digits)",
                material.slot
            ));
        }
    }
    for (name, slots) in by_name {
        if slots.len() > 1 {
            issue(format!(
                "{} of its materials are named {name}: slots {}. Rename one so each name is unique",
                slots.len(),
                slots.join(" and ")
            ));
        }
    }
}

/// What a model item exporting from `fbx` is, by its facts (C3), with what refuses it: an FBX
/// that can't be read, has two materials of one name, or has no kept mesh and no take, and the
/// build-order guards on models and animations.
fn read_fbx(fbx: &ItemFbx, item_path: &str, issue: &mut impl FnMut(String)) -> Kind {
    let what = if fbx.path == item_path {
        "FBX".to_string()
    } else {
        format!("its FBX {}", fbx.path)
    };
    let facts = match &fbx.facts {
        Ok(facts) => facts,
        Err(e) => {
            issue(format!("{what} can't be read for export: {e}"));
            return Kind::Model;
        }
    };
    for (name, count) in facts.duplicate_slots() {
        let count = if count == 2 {
            "two".to_string()
        } else {
            count.to_string()
        };
        issue(format!("{what} has {count} materials named {name}"));
    }
    match facts.reading() {
        Some(Reading::Model) => {
            issue("model export not built yet (an FBX with a kept mesh exports as a model)".into());
            Kind::Model
        }
        Some(Reading::Animation) => {
            issue("animation export not built yet (an FBX with no kept mesh and a take exports as an animation)".into());
            Kind::Animation
        }
        None => {
            issue(format!("{what} has no kept mesh and no take"));
            Kind::Model
        }
    }
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
/// asset in it. A source whose layout isn't one role (ORM, MaskMap, Roughness, Gloss, Metallic)
/// has no output role until texture conversion makes one, so it collides with nothing: a set's
/// `_R` and `_M` maps share a name because they merge into one MetallicSmoothness.
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
        match (row.kind, row.role) {
            (Kind::Texture, Some(role)) => by_key
                .entry((true, name.clone(), Some(role)))
                .or_default()
                .push(row),
            (Kind::Model | Kind::Animation, _) => by_key
                .entry((false, name.clone(), None))
                .or_default()
                .push(row),
            (Kind::Texture | Kind::Other, _) => {}
        }
    }
    for ((texture, name, role), rows) in by_key {
        if rows.len() < 2 {
            continue;
        }
        let what = match role.filter(|_| texture) {
            Some(role) => format!("textures are named {name} with the role {}", role.as_str()),
            None => format!("models or animations are named {name}"),
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

/// One asset's manifest entry, with the asset it is and its pack's index in the domain's packs.
struct Entry<'a> {
    value: Value,
    asset: &'a AssetRef,
    pack: usize,
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
    let mut entries: Vec<Entry> = Vec::new();
    for read in reads {
        let (Some(texture), Some(pack)) = (&read.ready, read.pack) else {
            continue;
        };
        if refused.contains(read.row.asset.asset_id.as_str()) {
            continue;
        }
        let role = texture.role.as_str();
        entries.push(Entry {
            value: json!({
                "kind": "texture",
                "assetName": texture.name,
                "file": format!("Textures/{}_{role}.{}", texture.name, texture.extension),
                "sha256": texture.sha256,
                "pack": export_packs[pack].id,
                "sourcePath": texture.source_path,
                "conversion": texture.conversion,
                "role": role,
            }),
            asset: &read.row.asset,
            pack,
        });
    }
    let kind_rank = |v: &Value| match v["kind"].as_str() {
        Some("model") => 0,
        Some("texture") => 1,
        _ => 2,
    };
    entries.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                kind_rank(v),
                v["assetName"].as_str().map(str::to_string),
                v["role"].as_str().map(str::to_string),
            )
        };
        key(&a.value).cmp(&key(&b.value))
    });
    validated(domain, entries, export_packs)
}

/// Validates the manifest of `entries` and drops every asset an error names, with the packs only
/// it used, until what is left passes, so the manifest holds exactly the assets that are ready.
/// Returns that manifest and the problems found on the way.
fn validated(
    domain: &str,
    mut entries: Vec<Entry>,
    export_packs: &[packs::ExportPack],
) -> (Value, Vec<Problem>) {
    let exported_at = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
    let mut found: Vec<Problem> = Vec::new();
    loop {
        let used: std::collections::BTreeSet<usize> = entries.iter().map(|e| e.pack).collect();
        let mut packs: Vec<&packs::ExportPack> = used.iter().map(|p| &export_packs[*p]).collect();
        packs.sort_by(|a, b| a.id.cmp(&b.id));
        let manifest = json!({
            "contractVersion": contract::CONTRACT_VERSION,
            "domain": domain,
            "exportedAt": exported_at,
            "exporter": { "name": "OpenDAM", "version": env!("CARGO_PKG_VERSION") },
            "packs": packs.iter().map(|p| p.manifest_entry()).collect::<Vec<_>>(),
            "assets": entries.iter().map(|e| e.value.clone()).collect::<Vec<_>>(),
        });

        let assets: Vec<&AssetRef> = entries.iter().map(|e| e.asset).collect();
        let problems = invalid(&manifest, domain, &assets);
        let named: HashSet<String> = problems
            .iter()
            .flat_map(|p| &p.assets)
            .map(|a| a.asset_id.clone())
            .collect();
        for problem in problems {
            if !found.contains(&problem) {
                found.push(problem);
            }
        }
        // Each round drops at least one asset, so this ends
        if named.is_empty() {
            return (manifest, found);
        }
        entries.retain(|e| !named.contains(&e.asset.asset_id));
    }
}

/// The problems of `manifest` that schema validation and the importer's rules (C1) find, each
/// naming the asset at its index in `assets`, the manifest's assets in order. An error about the
/// domain is left out when the domain's name isn't PascalCase, which is a problem already.
fn invalid(manifest: &Value, domain: &str, assets: &[&AssetRef]) -> Vec<Problem> {
    let errors = [
        contract::validate_manifest(manifest),
        contract::check_rules(manifest),
    ];
    let reported = |e: &String| !is_pascal(domain) && contract::is_domain_error(e);
    errors
        .into_iter()
        .filter_map(Result::err)
        .flatten()
        .filter(|e| !reported(e))
        .map(|error| {
            let asset = contract::asset_index(&error)
                .and_then(|i| assets.get(i))
                .map(|asset| (*asset).clone());
            Problem {
                message: match &asset {
                    Some(asset) => {
                        format!("{}: the manifest is invalid at {error}", asset.file_path)
                    }
                    None => format!("The manifest is invalid at {error}"),
                },
                assets: asset.into_iter().collect(),
            }
        })
        .collect()
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
    use crate::export::infer::{Surface, Via};
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

    /// `fbx_facts` as the worker answers it: for the fixture's FBX files from `fbx_facts.json`,
    /// which the worker's pytest checks against the files, and for files a test adds, what it
    /// gives `with`. It records the files it reads, relative to their library root.
    struct Facts {
        by_path: HashMap<String, Result<FbxFacts, String>>,
        read: std::sync::Mutex<Vec<String>>,
    }

    impl Facts {
        fn fixture() -> Self {
            let snapshot: HashMap<String, FbxFacts> = serde_json::from_str(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/game-export/fbx_facts.json"
            )))
            .unwrap();
            Facts {
                by_path: snapshot.into_iter().map(|(k, v)| (k, Ok(v))).collect(),
                read: Default::default(),
            }
        }

        fn with(mut self, path: &str, facts: Result<FbxFacts, String>) -> Self {
            self.by_path.insert(path.into(), facts);
            self
        }

        fn read_files(&self) -> Vec<String> {
            let mut read = self.read.lock().unwrap().clone();
            read.sort();
            read
        }
    }

    impl fbx_facts::Reader for Facts {
        async fn read(&self, source: &Path, root: &Path) -> Result<FbxFacts, String> {
            let path = source.strip_prefix(root).unwrap().display().to_string();
            self.read.lock().unwrap().push(path.clone());
            self.by_path
                .get(&path)
                .cloned()
                .unwrap_or_else(|| Err(format!("no facts for {path}")))
        }
    }

    /// Writes `bytes` at `path` in the fixture library and indexes it.
    async fn add_file(f: &Fixture, path: &str, bytes: &[u8]) {
        let target = f.library.root_path.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, bytes).unwrap();
        let root = f.library.root_path.display().to_string();
        crate::watcher::index_paths(&f.db, "fixture", &[root])
            .await
            .unwrap();
    }

    fn fixture_bytes(path: &str) -> Vec<u8> {
        std::fs::read(Path::new(FIXTURE).join(path)).unwrap()
    }

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
        check(&f.db, "domain", &Facts::fixture()).await.unwrap()
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
        let again = check(&f.db, "domain", &Facts::fixture()).await.unwrap();
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
                "Converted/Ruins/T_Pillar_N.png",
                "Dungeon Props/Rocks/T_Slate_ORM.png",
                "Dungeon Props/Textures/T_Wall_N_DX.png",
                "Dungeon Props/Textures/T_Wall_D.jpg",
                "Dungeon Props/Models/SM_Chest.fbx",
            ],
        )
        .await;

        assert_eq!(
            messages(&report),
            [
                "Converted/Ruins/T_Pillar_N.png: texture conversion not built yet: it needs its DirectX normal map's green channel flipped",
                "Dungeon Props/Models/SM_Chest.fbx: model export not built yet (an FBX with a kept mesh exports as a model)",
                "Dungeon Props/Rocks/T_Slate_ORM.png: texture conversion not built yet: it needs its ORM channels split into URP roles",
                "Dungeon Props/Textures/T_Wall_D.jpg: texture conversion not built yet: it needs its .jpg format to PNG",
                "Dungeon Props/Textures/T_Wall_N_DX.png: texture conversion not built yet: it needs its DirectX normal map's green channel flipped",
            ]
        );
        for problem in &report.problems {
            let path = problem.message.split(':').next().unwrap();
            assert_eq!(problem.assets.len(), 1);
            assert_eq!(problem.assets[0].file_path, path);
        }
        let members = report.assets.iter().filter(|a| a.member);
        assert!(members.clone().all(|a| a.status == Status::Problem));
        assert_eq!(members.count(), 5);
        // T_Pillar_N brings in its set's base color, which passes through
        assert_eq!(
            texture_entries(&report),
            [(
                "Pillar".to_string(),
                "BaseColor".to_string(),
                "Textures/Pillar_BaseColor.png".to_string()
            )]
        );
        assert_valid(&report);
        let ruins = report.packs.iter().find(|p| p.pack.id == "ruins").unwrap();
        assert_eq!(ruins.pack.origin, packs::Origin::Unreal);
        assert_eq!(ruins.normal_convention, Convention::DirectX);
        assert_eq!(ruins.pack.licence.spdx.as_deref(), Some("CC-BY-4.0"));
    }

    /// Saves your origin and normal-map convention for the fixture's pack at `path`, as the pack
    /// edit form does.
    async fn override_pack(
        f: &Fixture,
        path: &str,
        origin: Option<packs::Origin>,
        normal_convention: Option<Convention>,
    ) {
        let edit = crate::commands::folder::PackEdit::Details {
            name: None,
            license: None,
            source_url: None,
            origin,
            normal_convention,
        };
        crate::commands::folder::set_pack_override_in(&f.db, "fixture", path, edit)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn your_pack_origin_and_normal_convention_win_over_the_inferred_ones() {
        let f = fixture().await;
        override_pack(&f, "Dungeon Props", None, Some(Convention::DirectX)).await;
        override_pack(&f, "Unity Moss", Some(packs::Origin::Unreal), None).await;
        override_pack(&f, "Converted/Ruins", None, Some(Convention::OpenGl)).await;
        let report = check_domain(
            &f,
            &[
                "Converted/Ruins/T_Pillar_N.png",
                "Dungeon Props/Textures/T_StoneFloor_N.png",
                "Unity Moss/T_Moss_D.png",
            ],
        )
        .await;

        // Dungeon Props' normal maps are DirectX now, and Ruins' OpenGL, so the pillar's normal
        // map passes through, with its conversion.json's steps
        assert_eq!(
            messages(&report),
            ["Dungeon Props/Textures/T_StoneFloor_N.png: texture conversion not built yet: it needs its DirectX normal map's green channel flipped"]
        );
        assert!(
            texture_entries(&report).contains(&(
                "Pillar".to_string(),
                "Normal".to_string(),
                "Textures/Pillar_Normal.png".to_string()
            )),
            "{:?}",
            texture_entries(&report)
        );
        let packs: Vec<(&str, packs::Origin, Convention)> = report
            .packs
            .iter()
            .map(|p| (p.path.as_str(), p.pack.origin, p.normal_convention))
            .collect();
        assert_eq!(
            packs,
            [
                ("Converted/Ruins", packs::Origin::Unreal, Convention::OpenGl),
                ("Dungeon Props", packs::Origin::Generic, Convention::DirectX),
                // Your origin sets the convention's default
                ("Unity Moss", packs::Origin::Unreal, Convention::DirectX),
            ]
        );
        let ruins = report.manifest["packs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "ruins")
            .unwrap();
        assert_eq!(ruins["origin"], "unreal");
        assert_valid(&report);

        // The form shows the inferred origin beside yours
        let inferred = |path: &'static str| inferred_origin(&f.library, path);
        assert_eq!(inferred("Unity Moss").await, Ok(Some(packs::Origin::Unity)));
        assert_eq!(
            inferred("Converted/Ruins").await,
            Ok(Some(packs::Origin::Unreal))
        );
        assert_eq!(
            inferred("Dungeon Props").await,
            Ok(Some(packs::Origin::Generic))
        );
        assert_eq!(inferred("Dungeon Props/Textures").await, Ok(None));

        // Back to inferred
        override_pack(&f, "Unity Moss", None, None).await;
        let again = check(&f.db, "domain", &Facts::fixture()).await.unwrap();
        let moss = again.packs.iter().find(|p| p.path == "Unity Moss").unwrap();
        assert_eq!(
            (moss.pack.origin, moss.normal_convention),
            (packs::Origin::Unity, Convention::OpenGl)
        );
    }

    #[tokio::test]
    async fn textures_carry_their_nearest_conversion_json_steps() {
        let f = fixture().await;
        let report = check_domain(
            &f,
            &[
                "Converted/Ruins/T_Pillar_D.png",
                "Scans/T_Boulder_D.png",
                "Scans/Cliffs/T_Cliff_D.png",
                "Kenney Crates/crate_wood.png",
            ],
        )
        .await;

        // The step-3 guard's case now exports (AC14). T_Pillar_D brings in its set's normal map,
        // which is DirectX in an unreal pack and waits for texture conversion
        assert_eq!(
            messages(&report),
            ["Converted/Ruins/T_Pillar_N.png: texture conversion not built yet: it needs its DirectX normal map's green channel flipped"]
        );
        assert_valid(&report);
        let assets = report.manifest["assets"].as_array().unwrap();
        let chains: Vec<(&str, &Value)> = assets
            .iter()
            .map(|a| (a["assetName"].as_str().unwrap(), &a["conversion"]))
            .collect();
        let uasset = json!([{"tool": "UE Asset Exporter", "toolVersion": "5.4", "from": "uasset"}]);
        let sampler = json!([{
            "tool": "Substance 3D Sampler", "toolVersion": "4.2", "from": "jpg",
            "settings": {"resolution": 2048}
        }]);
        // The deeper Scans/Cliffs/conversion.json replaces the pack's, in its own order
        let cliffs = json!([
            {"tool": "RealityScan", "toolVersion": "2.0", "from": "jpg"},
            {
                "tool": "Substance 3D Sampler", "toolVersion": "4.2", "by": "delight",
                "from": "exr", "settings": {"resolution": 4096, "tiling": true}
            }
        ]);
        assert_eq!(
            chains,
            [
                ("Boulder", &sampler),
                ("Cliff", &cliffs),
                ("CrateWood", &json!([])),
                ("Pillar", &uasset),
            ]
        );
        assert_eq!(assets[1]["sourcePath"], "Cliffs/T_Cliff_D.png");

        // A conversion.json above the pack folder with a uasset step makes the pack unreal (AC9)
        let origins: Vec<(&str, packs::Origin)> = report
            .packs
            .iter()
            .map(|p| (p.pack.id.as_str(), p.pack.origin))
            .collect();
        assert_eq!(
            origins,
            [
                ("ruins", packs::Origin::Unreal),
                ("kenney-crates", packs::Origin::Generic),
                ("scans", packs::Origin::Generic),
            ]
        );
    }

    #[tokio::test]
    async fn an_invalid_conversion_json_is_one_problem_naming_it_and_its_files() {
        let f = fixture().await;
        let invalid =
            json!({"contractVersion": 1, "steps": [{"tool": "RealityScan", "from": "jpg"}]});
        std::fs::write(
            f.library.root_path.join("Scans/Cliffs/conversion.json"),
            invalid.to_string(),
        )
        .unwrap();
        std::fs::copy(
            f.library.root_path.join("Scans/Cliffs/T_Cliff_D.png"),
            f.library.root_path.join("Scans/Cliffs/T_Ledge_D.png"),
        )
        .unwrap();
        crate::watcher::index_paths(
            &f.db,
            "fixture",
            &[f.library.root_path.display().to_string()],
        )
        .await
        .unwrap();
        let report = check_domain(
            &f,
            &[
                "Scans/T_Boulder_D.png",
                "Scans/Cliffs/T_Cliff_D.png",
                "Scans/Cliffs/T_Ledge_D.png",
            ],
        )
        .await;

        assert_eq!(report.problems.len(), 1, "{:?}", messages(&report));
        let problem = &report.problems[0];
        assert!(
            problem.message.starts_with(
                "Scans/Cliffs/conversion.json: this conversion.json is invalid, so no file it covers can be exported: /steps/0"
            ),
            "{}",
            problem.message
        );
        assert!(
            problem.message.contains("toolVersion"),
            "{}",
            problem.message
        );
        let named: Vec<&str> = problem
            .assets
            .iter()
            .map(|a| a.file_path.as_str())
            .collect();
        assert_eq!(
            named,
            ["Scans/Cliffs/T_Cliff_D.png", "Scans/Cliffs/T_Ledge_D.png"]
        );
        let statuses: Vec<(&str, Status)> = report
            .assets
            .iter()
            .map(|a| (a.asset.file_path.as_str(), a.status))
            .collect();
        assert_eq!(
            statuses,
            [
                ("Scans/T_Boulder_D.png", Status::Ready),
                ("Scans/Cliffs/T_Cliff_D.png", Status::Problem),
                ("Scans/Cliffs/T_Ledge_D.png", Status::Problem),
            ]
        );
        assert_eq!(texture_entries(&report).len(), 1);
        assert_valid(&report);
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
        let report = check(&f.db, "domain", &Facts::fixture()).await.unwrap();

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
                // The member brings in its whole set
                (
                    "Dungeon Props/Textures/T_StoneFloor_N.png",
                    Kind::Texture,
                    Some("StoneFloor")
                ),
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

        let report = check(&f.db, "lower", &Facts::fixture()).await.unwrap();
        assert_eq!(
            messages(&report),
            [
                "The domain's name props isn't PascalCase (a capital letter, then letters and digits): it is a folder of the bundle path and the manifest's domain",
                "Recent is a smart collection: a domain and its subcollections must be ordinary collections",
            ]
        );

        let report = check(&f.db, "nested", &Facts::fixture()).await.unwrap();
        assert_eq!(
            messages(&report),
            ["Nested is not a domain: domains are the collections directly inside the root collection DungeonGame"]
        );

        sqlx::query("UPDATE collections SET name = 'Dungeon Game' WHERE id = 'dg'")
            .execute(&f.db.master_pool)
            .await
            .unwrap();
        collection(&f, "props", "Props", "dg", &[]).await;
        let report = check(&f.db, "props", &Facts::fixture()).await.unwrap();
        assert_eq!(
            messages(&report),
            ["The root collection's name Dungeon Game isn't PascalCase (a capital letter, then letters and digits): it is a folder of the bundle path"]
        );

        assert!(check(&f.db, "missing", &Facts::fixture()).await.is_err());
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

        let report = check(&f.db, "props", &Facts::fixture()).await.unwrap();
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
        let report = check(&f.db, "props", &Facts::fixture()).await.unwrap();
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
        let report = check(&f.db, "domain", &Facts::fixture()).await.unwrap();

        assert_eq!(messages(&report), Vec::<&str>::new());
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
                    Status::Ready
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
        assert_eq!(manifest_packs, ["foo", "foo-2", "rocks"]);
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
                ("Rock", "rocks", "T_Rock_D.png"),
                ("TileA", "foo", "tile_a.png"),
                ("TileC", "foo-2", "tile_c.png")
            ]
        );
        assert_eq!(
            assets[0]["conversion"],
            json!([{"tool": "UE", "toolVersion": "5.4", "from": "uasset"}])
        );
        assert_eq!(assets[1]["conversion"], json!([]));
        assert_eq!(
            assets[2]["sha256"],
            hashes::sha256_file(&root.join("Foo Copy/tile_c.png")).unwrap()
        );
        // Each library memoizes its own files' hashes
        for library in [&f.library, &second] {
            let memo: Vec<String> =
                sqlx::query_scalar("SELECT file_path FROM file_hashes ORDER BY file_path")
                    .fetch_all(&library.pool)
                    .await
                    .unwrap();
            let expected: &[&str] = if library.id == "second" {
                &["Foo Copy/tile_c.png", "Unreal/Rocks/T_Rock_D.png"]
            } else {
                &["Foo A/tile_a.png"]
            };
            assert_eq!(memo, expected, "{}", library.id);
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

        let report = check(&f.db, "props", &Facts::fixture()).await.unwrap();
        assert_eq!(
            messages(&report),
            ["The root collection dg set in Settings no longer exists"]
        );
        assert_eq!(report.bundle_path, None);
    }

    #[tokio::test]
    async fn maps_that_merge_into_one_role_are_no_name_collision() {
        let f = fixture().await;
        let textures = f.library.root_path.join("Dungeon Props/Textures");
        let png = std::fs::read(textures.join("T_StoneFloor_D.png")).unwrap();
        for name in [
            "Bricks_Roughness.png",
            "Bricks_Metallic.png",
            "Bricks_MaskMap.png",
        ] {
            std::fs::write(textures.join(name), &png).unwrap();
        }
        let root = f.library.root_path.display().to_string();
        crate::watcher::index_paths(&f.db, "fixture", &[root])
            .await
            .unwrap();

        let report = check_domain(
            &f,
            &[
                "Dungeon Props/Textures/Bricks_Roughness.png",
                "Dungeon Props/Textures/Bricks_Metallic.png",
                "Dungeon Props/Textures/Bricks_MaskMap.png",
            ],
        )
        .await;

        // Each waits for texture conversion, and none is reported as sharing a name
        assert_eq!(
            messages(&report),
            [
                "Dungeon Props/Textures/Bricks_MaskMap.png: texture conversion not built yet: it needs its MaskMap channels split into URP roles",
                "Dungeon Props/Textures/Bricks_Metallic.png: texture conversion not built yet: it needs its Metallic map merged into a MetallicSmoothness",
                "Dungeon Props/Textures/Bricks_Roughness.png: texture conversion not built yet: it needs its Roughness map merged into a MetallicSmoothness",
            ]
        );
        assert_valid(&report);
    }

    #[test]
    fn manifest_errors_name_the_asset_they_are_about() {
        let asset = |id: &str| AssetRef {
            library_id: "lib".into(),
            asset_id: id.into(),
            file_path: format!("Pack/{id}.png"),
        };
        let (rock, moss) = (asset("rock"), asset("moss"));
        let texture = |name: &str, sha256: &str| {
            json!({
                "kind": "texture", "assetName": name, "file": format!("Textures/{name}_BaseColor.png"),
                "sha256": sha256, "pack": "pack", "sourcePath": format!("{name}.png"),
                "conversion": [], "role": "BaseColor"
            })
        };
        let manifest = |domain: &str| {
            json!({
                "contractVersion": 1, "domain": domain, "exportedAt": "2026-10-22T12:00:00Z",
                "exporter": {"name": "OpenDAM", "version": "0.1.0"},
                "packs": [{"id": "pack", "name": "Pack", "licence": {"spdx": null, "name": "Unknown"}, "origin": "generic"}],
                // A schema error at the first asset, a rule error at the second
                "assets": [texture("Rock", "BAD"), texture("Moss", &"0".repeat(64))]
            })
        };
        let mut broken = manifest("Props");
        broken["assets"][1]["pack"] = json!("nope");

        let problems = invalid(&broken, "Props", &[&rock, &moss]);
        let named: Vec<(&str, &str)> = problems
            .iter()
            .map(|p| (p.assets[0].asset_id.as_str(), p.message.as_str()))
            .collect();
        assert_eq!(named.len(), 2, "{named:?}");
        assert_eq!(named[0].0, "rock");
        assert!(named[0]
            .1
            .starts_with("Pack/rock.png: the manifest is invalid at /assets/0/sha256: "));
        assert_eq!(
            named[1],
            (
                "moss",
                "Pack/moss.png: the manifest is invalid at /assets/1/pack: no pack has the id nope"
            )
        );

        // A bad domain name is reported once, by the Check, not again by the schema
        let mut lower = manifest("props");
        lower["assets"][0]["sha256"] = json!("0".repeat(64));
        assert_eq!(invalid(&lower, "props", &[&rock, &moss]), []);
        let unnamed = invalid(&lower, "Props", &[&rock, &moss]);
        assert_eq!(unnamed.len(), 1);
        assert!(unnamed[0].assets.is_empty());
        assert!(unnamed[0]
            .message
            .starts_with("The manifest is invalid at /domain: "));
    }

    #[test]
    fn assets_the_manifest_validation_refuses_leave_the_manifest_with_their_packs() {
        let asset = |id: &str| AssetRef {
            library_id: "lib".into(),
            asset_id: id.into(),
            file_path: format!("Pack/{id}.png"),
        };
        let (rock, moss) = (asset("rock"), asset("moss"));
        let pack = |id: &str| packs::ExportPack {
            id: id.into(),
            name: id.into(),
            version: None,
            publisher: None,
            source_url: None,
            licence: packs::Licence {
                spdx: None,
                name: "Unknown".into(),
                url: None,
            },
            origin: packs::Origin::Generic,
        };
        let entry = |name: &str, sha256: &str, pack: usize, asset| Entry {
            value: json!({
                "pack": (["rocks", "moss"][pack]),
                "kind": "texture", "assetName": name, "file": format!("Textures/{name}_BaseColor.png"),
                "sha256": sha256, "sourcePath": format!("{name}.png"),
                "conversion": [], "role": "BaseColor"
            }),
            asset,
            pack,
        };
        let entries = vec![
            entry("Moss", &"0".repeat(64), 1, &moss),
            // A bad sha256: the schema refuses this asset
            entry("Rock", "BAD", 0, &rock),
        ];

        let (manifest, problems) = validated("Props", entries, &[pack("rocks"), pack("moss")]);

        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].assets, std::slice::from_ref(&rock));
        assert!(problems[0]
            .message
            .starts_with("Pack/rock.png: the manifest is invalid at /assets/1/sha256: "));
        assert_eq!(manifest["assets"].as_array().unwrap().len(), 1);
        assert_eq!(manifest["assets"][0]["assetName"], "Moss");
        assert_eq!(manifest["packs"], json!([pack("moss").manifest_entry()]));
        assert_eq!(contract::validate_manifest(&manifest), Ok(()));
        assert_eq!(contract::check_rules(&manifest), Ok(()));
    }

    /// `assets` as (path, kind, status), by path.
    fn kinds_of<'a>(assets: &[&'a CheckedAsset]) -> Vec<(&'a str, Kind, Status)> {
        let mut rows: Vec<(&str, Kind, Status)> = assets
            .iter()
            .map(|a| (a.asset.file_path.as_str(), a.kind, a.status))
            .collect();
        rows.sort_by_key(|r| r.0);
        rows
    }

    #[tokio::test]
    async fn fbx_groups_and_lone_fbx_files_are_models_or_animations_by_their_facts() {
        let f = fixture().await;
        // The mesh-less animation fixture comes with animation export; a copy stands in for it
        let idle = "Dungeon Props/Animations/A_Idle.fbx";
        add_file(&f, idle, &fixture_bytes("Dungeon Props/Models/SM_Hull.fbx")).await;
        let facts = Facts::fixture().with(
            idle,
            Ok(FbxFacts {
                bones: vec!["Hips".into()],
                takes: vec!["Idle".into()],
                ..FbxFacts::default()
            }),
        );
        collection(
            &f,
            "domain",
            "Props",
            "dg",
            &[
                idle,
                "Dungeon Props/Models/SK_Guard.fbx",
                "Dungeon Props/Models/SM_Pillar.fbx",
                "Dungeon Props/Models/SM_Gate.fbx",
                // A member of the SM_Chest group, which reads as its main item, the FBX
                "Dungeon Props/Models/SM_Chest.obj",
                "Dungeon Props/Models/SM_Barrel.obj",
            ],
        )
        .await;
        let report = check(&f.db, "domain", &facts).await.unwrap();

        assert_eq!(
            messages(&report),
            [
                "Dungeon Props/Animations/A_Idle.fbx: animation export not built yet (an FBX with no kept mesh and a take exports as an animation)",
                "Dungeon Props/Models/SK_Guard.fbx: model export not built yet (an FBX with a kept mesh exports as a model)",
                "Dungeon Props/Models/SM_Barrel.obj: it has no FBX file: a model exports from its group's FBX",
                "Dungeon Props/Models/SM_Chest.fbx: model export not built yet (an FBX with a kept mesh exports as a model)",
                "Dungeon Props/Models/SM_Gate.fbx: model export not built yet (an FBX with a kept mesh exports as a model)",
                "Dungeon Props/Models/SM_Pillar.fbx: model export not built yet (an FBX with a kept mesh exports as a model)",
                // Textures the models' materials bring in
                "Dungeon Props/Textures/T_Guard_AORM.png: texture conversion not built yet: it needs its ORM channels split into URP roles",
                "Dungeon Props/Textures/T_Iron_M.png: texture conversion not built yet: it needs its Metallic map merged into a MetallicSmoothness",
                "Dungeon Props/Textures/T_Iron_R.png: texture conversion not built yet: it needs its Roughness map merged into a MetallicSmoothness",
            ]
        );
        let models: Vec<_> = report
            .assets
            .iter()
            .filter(|a| a.kind != Kind::Texture)
            .collect();
        assert_eq!(
            kinds_of(&models),
            [
                (idle, Kind::Animation, Status::Problem),
                (
                    "Dungeon Props/Models/SK_Guard.fbx",
                    Kind::Model,
                    Status::Problem
                ),
                (
                    "Dungeon Props/Models/SM_Barrel.obj",
                    Kind::Model,
                    Status::Problem
                ),
                (
                    "Dungeon Props/Models/SM_Chest.fbx",
                    Kind::Model,
                    Status::Problem
                ),
                (
                    "Dungeon Props/Models/SM_Gate.fbx",
                    Kind::Model,
                    Status::Problem
                ),
                (
                    "Dungeon Props/Models/SM_Pillar.fbx",
                    Kind::Model,
                    Status::Problem
                ),
            ]
        );
        let idle_row = report
            .assets
            .iter()
            .find(|a| a.kind == Kind::Animation)
            .unwrap();
        assert_eq!(idle_row.asset_name.as_deref(), Some("Idle"));
        // Only FBX files are read, and no OBJ
        assert_eq!(
            facts.read_files(),
            [
                idle,
                "Dungeon Props/Models/SK_Guard.fbx",
                "Dungeon Props/Models/SM_Chest.fbx",
                "Dungeon Props/Models/SM_Gate.fbx",
                "Dungeon Props/Models/SM_Pillar.fbx",
            ]
        );
        // No model exports yet, but the textures their materials bring in that pass through do
        let names: Vec<(String, String)> = texture_entries(&report)
            .into_iter()
            .map(|(name, role, _)| (name, role))
            .collect();
        let expected = [
            ("Gate", "BaseColor"),
            ("Guard", "BaseColor"),
            ("Pillar", "BaseColor"),
            ("Pillar", "Normal"),
        ];
        assert_eq!(names, expected.map(|(n, r)| (n.to_string(), r.to_string())));
        assert_valid(&report);
    }

    #[tokio::test]
    async fn an_fbx_with_nothing_to_export_or_two_materials_of_one_name_is_refused_by_name() {
        let f = fixture().await;
        let broken = "Dungeon Props/Models/SM_Broken.fbx";
        add_file(&f, broken, b"not an FBX").await;
        let facts =
            Facts::fixture().with(broken, Err("can't read the FBX: not an FBX file".into()));
        collection(
            &f,
            "domain",
            "Props",
            "dg",
            &[
                broken,
                "Dungeon Props/Models/SM_Hull.fbx",
                "Dungeon Props/Models/SM_Bench.fbx",
                // Material, a real DefaultMaterial and OpenDAMDefault are ordinary slots
                "Dungeon Props/Models/SM_Stool.fbx",
                "Dungeon Props/Models/SM_Lamp.fbx",
                "Dungeon Props/Models/SM_Shelf.fbx",
            ],
        )
        .await;
        let report = check(&f.db, "domain", &facts).await.unwrap();

        let guard = "model export not built yet (an FBX with a kept mesh exports as a model)";
        let expected = [
            "Dungeon Props/Models/SM_Bench.fbx: FBX has two materials named Wood".to_string(),
            format!("Dungeon Props/Models/SM_Bench.fbx: {guard}"),
            "Dungeon Props/Models/SM_Broken.fbx: FBX can't be read for export: can't read the FBX: not an FBX file".to_string(),
            "Dungeon Props/Models/SM_Hull.fbx: FBX has no kept mesh and no take".to_string(),
            format!("Dungeon Props/Models/SM_Lamp.fbx: {guard}"),
            format!("Dungeon Props/Models/SM_Shelf.fbx: {guard}"),
            format!("Dungeon Props/Models/SM_Stool.fbx: {guard}"),
            // SM_Stool's slot finds its JPG base color by the model's name
            "Dungeon Props/Textures/T_Stool_D.jpg: texture conversion not built yet: it needs its .jpg format to PNG".to_string(),
        ];
        assert_eq!(messages(&report), expected);
        for problem in &report.problems {
            let path = problem.message.split(':').next().unwrap();
            assert_eq!(problem.assets.len(), 1);
            assert_eq!(problem.assets[0].file_path, path);
        }
        let models = report.assets.iter().filter(|a| a.kind == Kind::Model);
        assert_eq!(models.count(), 6);
        assert_valid(&report);
    }

    #[tokio::test]
    async fn a_group_exports_from_its_unity_fbx_and_names_it_in_its_problems() {
        let f = fixture().await;
        let fbx = fixture_bytes("Dungeon Props/Models/SM_Bench.fbx");
        add_file(&f, "Kit/datapackage.json", br#"{"name": "kit"}"#).await;
        add_file(&f, "Kit/Assets/gltf/crate.gltf", b"{}").await;
        add_file(&f, "Kit/Assets/fbx/crate.fbx", &fbx).await;
        add_file(&f, "Kit/Assets/FBX (Unity)/crate.fbx", &fbx).await;
        let unity = "Kit/Assets/FBX (Unity)/crate.fbx";
        let facts = Facts::fixture().with(
            unity,
            Ok(serde_json::from_value(json!({
                "slots": [{"name": "Wood"}, {"name": "Wood"}, {"name": "Wood"}],
                "kept_meshes": ["crate"], "dropped_meshes": [], "has_kept_mesh": false,
                "kept_mesh_without_material": false, "bones": [], "takes": [],
            }))
            .unwrap()),
        );
        let report = check_domain_with(&f, &["Kit/Assets/gltf/crate.gltf"], &facts).await;

        // The glTF is the main item; the Unity FBX is the file it exports from
        assert_eq!(
            messages(&report),
            [
                "Kit/Assets/gltf/crate.gltf: its FBX Kit/Assets/FBX (Unity)/crate.fbx has 3 materials named Wood",
                "Kit/Assets/gltf/crate.gltf: its FBX Kit/Assets/FBX (Unity)/crate.fbx has no kept mesh and no take",
            ]
        );
        assert_eq!(facts.read_files(), [unity]);
    }

    /// `Facts` that answers only once `reads` reads are in flight together.
    struct AllAtOnce {
        facts: Facts,
        together: tokio::sync::Barrier,
    }

    impl fbx_facts::Reader for AllAtOnce {
        async fn read(&self, source: &Path, root: &Path) -> Result<FbxFacts, String> {
            self.together.wait().await;
            self.facts.read(source, root).await
        }
    }

    #[tokio::test]
    async fn a_libraries_fbx_files_are_read_all_at_once() {
        let f = fixture().await;
        let paths = [
            "Dungeon Props/Models/SM_Pillar.fbx",
            "Dungeon Props/Models/SM_Gate.fbx",
            "Dungeon Props/Models/SM_Hull.fbx",
            "Dungeon Props/Models/SM_Stool.fbx",
            "Dungeon Props/Models/SK_Guard.fbx",
        ];
        collection(&f, "domain", "Props", "dg", &paths).await;
        let facts = AllAtOnce {
            facts: Facts::fixture(),
            together: tokio::sync::Barrier::new(paths.len()),
        };

        // Read one after another, the first read would wait for the others for ever
        let report = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            check(&f.db, "domain", &facts),
        )
        .await
        .expect("every read is in flight at once")
        .unwrap();
        assert_eq!(facts.facts.read_files().len(), paths.len());
        assert!(messages(&report)
            .contains(&"Dungeon Props/Models/SM_Hull.fbx: FBX has no kept mesh and no take"));
    }

    /// Checks a new domain holding `paths`, reading FBX facts from `facts`.
    async fn check_domain_with(f: &Fixture, paths: &[&str], facts: &Facts) -> CheckReport {
        collection(f, "domain", "Props", "dg", paths).await;
        check(&f.db, "domain", facts).await.unwrap()
    }

    type MaterialRow = (
        String,
        String,
        Surface,
        bool,
        bool,
        Vec<(Role, String, Via)>,
    );

    /// The materials of the model at `path` in `report`, as (slot, name, surface, cutout
    /// suggested, added, [(role, texture name, via)]).
    fn materials_of(report: &CheckReport, path: &str) -> Vec<MaterialRow> {
        let model = report
            .assets
            .iter()
            .find(|a| a.asset.file_path == path)
            .unwrap_or_else(|| panic!("{path} isn't listed"));
        model
            .materials
            .iter()
            .map(|m| {
                let textures = m
                    .textures
                    .iter()
                    .map(|(role, b)| (*role, b.asset_name.clone(), b.via))
                    .collect();
                (
                    m.slot.clone(),
                    m.name.clone(),
                    m.surface,
                    m.cutout_suggested,
                    m.added,
                    textures,
                )
            })
            .collect()
    }

    fn material(
        slot: &str,
        name: &str,
        surface: Surface,
        suggested: bool,
        added: bool,
        textures: &[(Role, &str, Via)],
    ) -> MaterialRow {
        (
            slot.into(),
            name.into(),
            surface,
            suggested,
            added,
            textures
                .iter()
                .map(|(role, name, via)| (*role, name.to_string(), *via))
                .collect(),
        )
    }

    type TextureRow<'a> = (&'a str, &'a str, &'a str, Option<Convention>, bool, Status);

    /// `report`'s texture rows as (path, name, layout, convention, member, status), by path.
    fn texture_rows(report: &CheckReport) -> Vec<TextureRow<'_>> {
        let mut rows: Vec<_> = report
            .assets
            .iter()
            .filter(|a| a.kind == Kind::Texture)
            .map(|a| {
                (
                    a.asset.file_path.as_str(),
                    a.asset_name.as_deref().unwrap(),
                    a.layout.unwrap(),
                    a.normal_convention,
                    a.member,
                    a.status,
                )
            })
            .collect();
        rows.sort_by_key(|r| r.0);
        rows
    }

    #[tokio::test]
    async fn models_materials_are_inferred_and_their_textures_join_the_domain() {
        use Role::*;
        use Surface::*;
        use Via::*;
        let f = fixture().await;
        let report = check_domain(
            &f,
            &[
                "Dungeon Props/Models/SM_Pillar.fbx",
                "Dungeon Props/Models/SM_Gate.fbx",
                "Dungeon Props/Models/SK_Guard.fbx",
                "Dungeon Props/Models/SM_Crate.fbx",
                "Dungeon Props/Models/SM_Boulder.fbx",
                "Dungeon Props/Models/SM_Lamp.fbx",
                "Dungeon Props/Models/SM_Shelf.fbx",
                "Dungeon Props/Models/SM_Stool.fbx",
                "Unity Moss/Models/SM_Log.fbx",
                // A member a material uses too: one asset
                "Dungeon Props/Textures/T_StoneFloor_D.png",
            ],
        )
        .await;

        let models = "Dungeon Props/Models";
        let expected: [(&str, Vec<MaterialRow>); 9] = [
            // The FBX's own references, resolved in the pack; Moss finds nothing
            (
                "SM_Pillar.fbx",
                vec![
                    material(
                        "Stone",
                        "Stone",
                        Opaque,
                        false,
                        false,
                        &[(BaseColor, "Pillar", Fbx), (Normal, "Pillar", Fbx)],
                    ),
                    material("Moss", "Moss", Opaque, false, false, &[]),
                ],
            ),
            // Suffixes on images named like the slot
            (
                "SM_Gate.fbx",
                vec![material(
                    "Iron",
                    "Iron",
                    Opaque,
                    false,
                    false,
                    &[
                        (BaseColor, "Gate", Suffix),
                        (MetallicSmoothness, "Gate", Suffix),
                    ],
                )],
            ),
            // ... and like the model; its base color's cut-out alpha is only a suggestion
            (
                "SK_Guard.fbx",
                vec![material(
                    "Cloth",
                    "Cloth",
                    Opaque,
                    true,
                    false,
                    &[
                        (BaseColor, "Guard", Suffix),
                        (MetallicSmoothness, "Guard", Suffix),
                        (Occlusion, "Guard", Suffix),
                    ],
                )],
            ),
            (
                "SM_Crate.fbx",
                vec![material(
                    "Planks",
                    "Planks",
                    Opaque,
                    false,
                    false,
                    &[(Normal, "Crate", Suffix)],
                )],
            ),
            // The slot the export adds, with an image of the model's stem as its base color
            (
                "SM_Boulder.fbx",
                vec![material(
                    "OpenDAMDefault",
                    "OpenDAMDefault",
                    Opaque,
                    false,
                    true,
                    &[(BaseColor, "Boulder", Stem)],
                )],
            ),
            (
                "SM_Lamp.fbx",
                vec![
                    material(
                        "DefaultMaterial",
                        "DefaultMaterial",
                        Opaque,
                        false,
                        false,
                        &[],
                    ),
                    material("OpenDAMDefault", "OpenDAMDefault", Opaque, false, true, &[]),
                ],
            ),
            (
                "SM_Shelf.fbx",
                vec![
                    material(
                        "OpenDAMDefault",
                        "OpenDAMDefault",
                        Opaque,
                        false,
                        false,
                        &[],
                    ),
                    material(
                        "OpenDAMDefault2",
                        "OpenDAMDefault2",
                        Opaque,
                        false,
                        true,
                        &[],
                    ),
                ],
            ),
            (
                "SM_Stool.fbx",
                vec![material(
                    "Material",
                    "Material",
                    Opaque,
                    false,
                    false,
                    &[(BaseColor, "Stool", Suffix)],
                )],
            ),
            // A Unity pack's materials; two sets in BaseColor take the materials' names
            (
                "../../Unity Moss/Models/SM_Log.fbx",
                vec![
                    material(
                        "Bark",
                        "Bark",
                        Opaque,
                        false,
                        false,
                        &[(BaseColor, "LogBark", Unity), (Normal, "LogBark", Unity)],
                    ),
                    material(
                        "Leaves",
                        "Leaves",
                        Cutout,
                        false,
                        false,
                        &[(BaseColor, "LogLeaves", Unity)],
                    ),
                ],
            ),
        ];
        for (file, materials) in expected {
            let path = match file.strip_prefix("../../") {
                Some(path) => path.to_string(),
                None => format!("{models}/{file}"),
            };
            assert_eq!(materials_of(&report, &path), materials, "{path}");
        }
        let crate_ = report
            .assets
            .iter()
            .find(|a| a.asset.file_path.ends_with("SM_Crate.fbx"))
            .unwrap();
        assert_eq!(
            crate_.materials[0].textures[&Normal].files,
            ["Dungeon Props/Textures/1K/T_Crate_N.png"]
        );

        use Status::*;
        let dx = None;
        assert_eq!(
            texture_rows(&report),
            [
                (
                    "Dungeon Props/Models/SM_Boulder.png",
                    "Boulder",
                    "BaseColor",
                    dx,
                    false,
                    Ready
                ),
                (
                    "Dungeon Props/Textures/1K/T_Crate_N.png",
                    "Crate",
                    "Normal",
                    Some(Convention::OpenGl),
                    false,
                    Ready
                ),
                (
                    "Dungeon Props/Textures/T_Guard_AORM.png",
                    "Guard",
                    "ORM",
                    dx,
                    false,
                    Problem
                ),
                (
                    "Dungeon Props/Textures/T_Guard_D.png",
                    "Guard",
                    "BaseColor",
                    dx,
                    false,
                    Ready
                ),
                (
                    "Dungeon Props/Textures/T_Iron_D.png",
                    "Gate",
                    "BaseColor",
                    dx,
                    false,
                    Ready
                ),
                (
                    "Dungeon Props/Textures/T_Iron_M.png",
                    "Gate",
                    "Metallic",
                    dx,
                    false,
                    Problem
                ),
                (
                    "Dungeon Props/Textures/T_Iron_R.png",
                    "Gate",
                    "Roughness",
                    dx,
                    false,
                    Problem
                ),
                (
                    "Dungeon Props/Textures/T_StoneFloor_D.png",
                    "Pillar",
                    "BaseColor",
                    dx,
                    true,
                    Ready
                ),
                (
                    "Dungeon Props/Textures/T_StoneFloor_N.png",
                    "Pillar",
                    "Normal",
                    Some(Convention::OpenGl),
                    false,
                    Ready
                ),
                (
                    "Dungeon Props/Textures/T_Stool_D.jpg",
                    "Stool",
                    "BaseColor",
                    dx,
                    false,
                    Problem
                ),
                (
                    "Unity Moss/Textures/T_Bark_Albedo.png",
                    "LogBark",
                    "BaseColor",
                    dx,
                    false,
                    Ready
                ),
                (
                    "Unity Moss/Textures/T_Bark_Normal.png",
                    "LogBark",
                    "Normal",
                    Some(Convention::OpenGl),
                    false,
                    Ready
                ),
                (
                    "Unity Moss/Textures/T_LeafAtlas.png",
                    "LogLeaves",
                    "BaseColor",
                    dx,
                    false,
                    Ready
                ),
            ]
        );

        // The step-5 guard names each texture that needs converting (AC14)
        let refused: Vec<&str> = messages(&report)
            .into_iter()
            .filter(|m| m.contains("texture conversion not built yet"))
            .collect();
        assert_eq!(
            refused,
            [
                "Dungeon Props/Textures/T_Guard_AORM.png: texture conversion not built yet: it needs its ORM channels split into URP roles",
                "Dungeon Props/Textures/T_Iron_M.png: texture conversion not built yet: it needs its Metallic map merged into a MetallicSmoothness",
                "Dungeon Props/Textures/T_Iron_R.png: texture conversion not built yet: it needs its Roughness map merged into a MetallicSmoothness",
                "Dungeon Props/Textures/T_Stool_D.jpg: texture conversion not built yet: it needs its .jpg format to PNG",
            ]
        );
        assert_valid(&report);
    }

    #[tokio::test]
    async fn a_set_used_by_several_models_keeps_its_stem() {
        let f = fixture().await;
        let report = check_domain(
            &f,
            &[
                "Dungeon Props/Models/SM_Pillar.fbx",
                "Dungeon Props/Models/SM_Totem.fbx",
            ],
        )
        .await;
        for model in ["SM_Pillar", "SM_Totem"] {
            let stone = &materials_of(&report, &format!("Dungeon Props/Models/{model}.fbx"))[0];
            assert_eq!(
                stone.5,
                [
                    (Role::BaseColor, "StoneFloor".to_string(), Via::Fbx),
                    (Role::Normal, "StoneFloor".to_string(), Via::Fbx)
                ],
                "{model}"
            );
        }
        let names: Vec<&str> = texture_rows(&report).into_iter().map(|r| r.1).collect();
        assert_eq!(names, ["StoneFloor", "StoneFloor"]);
    }

    #[tokio::test]
    async fn standalone_sets_join_whole_and_the_guard_names_each_source() {
        let f = fixture().await;
        let report = check_domain(
            &f,
            &[
                "Dungeon Props/Rocks/T_Rock_R.png",
                "Dungeon Props/Rocks/T_Ore_M.png",
                "Dungeon Props/Rocks/T_Slate_ORM.png",
                "Dungeon Props/Textures/T_StoneFloor_N.png",
            ],
        )
        .await;

        use Status::*;
        let dx = Some(Convention::DirectX);
        assert_eq!(
            texture_rows(&report),
            [
                (
                    "Dungeon Props/Rocks/T_Ore_M.png",
                    "Ore",
                    "Metallic",
                    None,
                    true,
                    Problem
                ),
                (
                    "Dungeon Props/Rocks/T_Rock_M.png",
                    "Rock",
                    "Metallic",
                    None,
                    false,
                    Problem
                ),
                // A `_DX` suffix is read before the role, in a pack whose maps are OpenGL
                (
                    "Dungeon Props/Rocks/T_Rock_N_DX.png",
                    "Rock",
                    "Normal",
                    dx,
                    false,
                    Problem
                ),
                (
                    "Dungeon Props/Rocks/T_Rock_R.png",
                    "Rock",
                    "Roughness",
                    None,
                    true,
                    Problem
                ),
                (
                    "Dungeon Props/Rocks/T_Slate_ORM.png",
                    "Slate",
                    "ORM",
                    None,
                    true,
                    Problem
                ),
                (
                    "Dungeon Props/Textures/T_StoneFloor_D.png",
                    "StoneFloor",
                    "BaseColor",
                    None,
                    false,
                    Ready
                ),
                (
                    "Dungeon Props/Textures/T_StoneFloor_N.png",
                    "StoneFloor",
                    "Normal",
                    Some(Convention::OpenGl),
                    true,
                    Ready
                ),
            ]
        );
        assert_eq!(
            messages(&report),
            [
                "Dungeon Props/Rocks/T_Ore_M.png: texture conversion not built yet: it needs its Metallic map merged into a MetallicSmoothness",
                "Dungeon Props/Rocks/T_Rock_M.png: texture conversion not built yet: it needs its Metallic map merged into a MetallicSmoothness",
                "Dungeon Props/Rocks/T_Rock_N_DX.png: texture conversion not built yet: it needs its DirectX normal map's green channel flipped",
                "Dungeon Props/Rocks/T_Rock_R.png: texture conversion not built yet: it needs its Roughness map merged into a MetallicSmoothness",
                "Dungeon Props/Rocks/T_Slate_ORM.png: texture conversion not built yet: it needs its ORM channels split into URP roles",
            ]
        );
        // The set whose sources all pass through exports whole
        assert_eq!(
            texture_entries(&report),
            [
                (
                    "StoneFloor".to_string(),
                    "BaseColor".to_string(),
                    "Textures/StoneFloor_BaseColor.png".to_string()
                ),
                (
                    "StoneFloor".to_string(),
                    "Normal".to_string(),
                    "Textures/StoneFloor_Normal.png".to_string()
                ),
            ]
        );
        assert_valid(&report);
    }

    #[tokio::test]
    async fn materials_of_one_model_with_one_name_are_refused_naming_the_slots() {
        let f = fixture().await;
        let table = "Dungeon Props/Models/SM_Table.fbx";
        add_file(
            &f,
            table,
            &fixture_bytes("Dungeon Props/Models/SM_Stool.fbx"),
        )
        .await;
        let facts = Facts::fixture().with(
            table,
            Ok(serde_json::from_value(json!({
                "slots": [{"name": "M_Wood"}, {"name": "Wood"}, {"name": "木"}],
                "kept_meshes": ["SM_Table"], "dropped_meshes": [], "has_kept_mesh": true,
                "kept_mesh_without_material": false, "bones": [], "takes": [],
            }))
            .unwrap()),
        );
        let report = check_domain_with(&f, &[table], &facts).await;

        assert_eq!(
            messages(&report),
            [
                "Dungeon Props/Models/SM_Table.fbx: model export not built yet (an FBX with a kept mesh exports as a model)",
                "Dungeon Props/Models/SM_Table.fbx: its material slot 木 gives no PascalCase name (a capital letter, then letters and digits)",
                "Dungeon Props/Models/SM_Table.fbx: 2 of its materials are named Wood: slots M_Wood and Wood. Rename one so each name is unique",
            ]
        );
        let names: Vec<(String, String)> = materials_of(&report, table)
            .into_iter()
            .map(|m| (m.0, m.1))
            .collect();
        assert_eq!(
            names,
            [("M_Wood", "Wood"), ("Wood", "Wood"), ("木", "")].map(|(s, n)| (s.into(), n.into()))
        );
    }

    #[test]
    fn the_exported_fbx_is_the_first_in_a_unity_folder_else_the_first_by_path() {
        assert_eq!(
            exported_fbx([
                "Kit/fbx/b.fbx",
                "Kit/FBX (Unity)/b.fbx",
                "Kit/fbx(unity)/b.fbx"
            ]),
            Some("Kit/FBX (Unity)/b.fbx")
        );
        assert_eq!(
            exported_fbx(["Kit/b/crate.fbx", "Kit/A/crate.fbx"]),
            Some("Kit/A/crate.fbx")
        );
        // Only a folder counts, not a file named like one
        assert!(!in_unity_fbx_folder("Kit/fbx(unity)"));
        assert_eq!(exported_fbx([]), None);
    }
}
