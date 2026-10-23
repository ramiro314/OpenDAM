//! The Export view's Check (plan game-export C14): reads a domain, builds its manifest in memory,
//! validates it, and lists every asset with its kind, default `assetName`, pack and status, and
//! every problem that would refuse the export. It also returns what the bundle module needs to
//! stage the bundle: the files to copy, and the fingerprint of everything it read (C14).
//!
//! A domain is a direct child of the root collection (C3). Its assets are the grid items in it
//! and all its subcollections, across every library, each once. A format group with an FBX, or
//! a lone FBX, is a model or an animation by its FBX's facts (`fbx_facts`). Until later steps of
//! the plan's build order land, only standalone textures that pass through byte for byte export,
//! each with its nearest `conversion.json`'s steps (C10): the guards refuse a texture that needs
//! converting, every model and every animation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};

use crate::db::{Database, LibraryHandle};
use crate::export::bundle::Diff;
use crate::export::conversion::{Invalid, Walker};
use crate::export::fbx_facts::{self, FbxFacts, Reading};
use crate::export::fingerprint::{Fingerprint, Probes, Stamp};
use crate::export::infer::{self, PackImages, UnityPack};
use crate::export::names::{is_pascal, pascal};
use crate::export::packs::{self, Origin, PackSource};
use crate::export::textures::{self, Convention, Layout, Refusal, Role, SourceEdits};
use crate::export::{contract, edits, hashes, settings};
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

/// An asset's status in the Export view: `Ready` when the previous bundle holds it as the next
/// one would, `Changed` or `New` when it doesn't, and `Problem` when a problem names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ready,
    Changed,
    New,
    Problem,
}

/// One row of the Export view's list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckedAsset {
    #[serde(flatten)]
    pub asset: AssetRef,
    pub kind: Kind,
    /// Its name: yours (C4), else its default (C5), `None` when it has none
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
    /// `<export root>/<root collection>/<domain>`, once both settings are set, the export root is
    /// outside every library, the domain is a child of the root collection, and both their names
    /// are PascalCase
    pub bundle_path: Option<String>,
    pub assets: Vec<CheckedAsset>,
    /// Every pack an asset of the domain is in, in manifest order
    pub packs: Vec<DomainPack>,
    pub problems: Vec<Problem>,
    /// The manifest of the assets that are ready, as the bundle would hold it
    pub manifest: Value,
    /// What exporting `manifest` changes in the bundle, once the bundle path is known
    pub diff: Option<Diff>,
    /// Whether the bundle is staged, so Export can swap it in
    pub staged: bool,
}

/// A file the bundle holds: its `file` in the manifest, the file to copy and its sha256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleFile {
    pub file: String,
    pub source: PathBuf,
    pub sha256: String,
    pub asset: AssetRef,
}

/// A Check before anything is staged: its report, the bundle's files, and the fingerprint of
/// everything it read.
#[derive(Debug, Clone)]
pub struct Inspection {
    pub report: CheckReport,
    pub files: Vec<BundleFile>,
    pub fingerprint: Fingerprint,
    /// Each listed asset's `assetName`, inferred and yours, by asset id: the Game tab's
    pub names: HashMap<String, Field<String>>,
    /// What the Game tab shows of each model with materials, by asset id
    pub models: HashMap<String, ModelGame>,
    /// What the Game tab shows of each texture source, by asset id
    pub textures: HashMap<String, TextureGame>,
}

/// An image as the Game tab offers or shows it: its layout and the roles it gives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackImage {
    pub asset_id: String,
    /// Relative to the library root
    pub file_path: String,
    /// Its layout's label: a role, or ORM, MaskMap, Roughness, Gloss or Metallic
    pub layout: &'static str,
    pub roles: Vec<Role>,
}

impl PackImage {
    fn of(images: &PackImages, image: usize) -> Self {
        let layout = images.layout(image);
        PackImage {
            asset_id: images.images[image].id.clone(),
            file_path: images.images[image].path.clone(),
            layout: layout.label(),
            roles: layout.outputs().to_vec(),
        }
    }
}

/// What the Game tab shows of a model (C13): its materials, inferred and with your edits, and
/// its pack's images, which its texture pickers offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelGame {
    pub materials: Vec<infer::Material>,
    /// By path
    pub images: Vec<PackImage>,
}

/// What the Game tab shows of a texture source (C13): its own layout, role and normal-map
/// convention, each inferred and yours, and its texture set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextureGame {
    /// As an edit names it: `passThrough` for one role, else ORM, MaskMap, Roughness, Gloss or
    /// Metallic
    pub layout: Field<&'static str>,
    /// Its role, when its layout passes one role through. Other layouts fix their roles (C4).
    pub role: Option<Field<Role>>,
    pub normal_convention: Field<Convention>,
    /// The sources of its texture set, itself included, by path
    pub set: Vec<PackImage>,
    /// The roles its set exports, one texture each
    pub outputs: Vec<Role>,
}

impl TextureGame {
    /// A source inferred as `layout` and `convention`, with your `edits`, in a set of `set`.
    fn new(
        layout: Layout,
        convention: Convention,
        edits: SourceEdits,
        set: Vec<PackImage>,
    ) -> Self {
        let role = edits.layout(layout).role().map(|_| Field {
            inferred: layout.role().unwrap_or(Role::BaseColor),
            yours: edits.role,
        });
        TextureGame {
            layout: Field {
                inferred: layout.edit_name(),
                yours: edits.layout.map(Layout::edit_name),
            },
            role,
            normal_convention: Field {
                inferred: convention,
                yours: edits.convention,
            },
            outputs: (Role::ALL.into_iter())
                .filter(|r| set.iter().any(|s| s.roles.contains(r)))
                .collect(),
            set,
        }
    }
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
pub(crate) struct ItemRow {
    pub id: String,
    pub file_path: String,
    pub file_name: String,
    pub extension: String,
    pub kind: String,
}

impl ItemRow {
    fn stem(&self) -> &str {
        self.file_name
            .rsplit_once('.')
            .map_or(self.file_name.as_str(), |(stem, _)| stem)
    }
}

/// A value the export infers, and yours when you set one, which wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Field<T> {
    pub inferred: T,
    pub yours: Option<T>,
}

impl<T: Clone> Field<T> {
    /// What the export uses: yours, else the inferred value
    pub fn value(&self) -> T {
        self.yours.clone().unwrap_or_else(|| self.inferred.clone())
    }
}

/// The `assetName` of grid item `item` among its library's edit rows `edits`: its default (C5),
/// and yours (C4). A model's or animation's is its own, a texture's its set's. `None` for an item
/// that exports as neither.
pub(crate) fn asset_name(item: &ItemRow, edits: &[edits::Row]) -> Option<Field<String>> {
    match AssetKind::from_str(&item.kind) {
        AssetKind::Model3d => Some(Field {
            inferred: pascal(item.stem()),
            yours: edits
                .iter()
                .find(|e| e.asset_id == item.id)
                .and_then(|e| edits::asset_name(&e.parsed())),
        }),
        _ if edits::is_image(&item.kind) => Some(Field {
            inferred: pascal(&textures::read_stem(item.stem()).base),
            yours: edits::set_name(edits, &item.file_path, &item.file_name),
        }),
        _ => None,
    }
}

/// What grid item `item` exports as (C3): a model item is a model, or an animation when the
/// facts of the FBX it exports from (`exported_fbx_of`) say so, an image is a texture, and
/// anything else is `Other`.
pub(crate) fn exported_kind(item: &ItemRow, facts: Option<&Result<FbxFacts, String>>) -> Kind {
    match AssetKind::from_str(&item.kind) {
        AssetKind::Model3d => match facts {
            Some(Ok(facts)) if facts.reading() == Some(Reading::Animation) => Kind::Animation,
            _ => Kind::Model,
        },
        _ if edits::is_image(&item.kind) => Kind::Texture,
        _ => Kind::Other,
    }
}

/// The FBX each of `items` exports from (A5), by item id, for each that is a format group with
/// an FBX member, or a lone FBX file, a group of one.
pub(crate) async fn exported_fbx_of(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    items: &[ItemRow],
) -> sqlx::Result<HashMap<String, String>> {
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
    for item in items {
        if item.extension.eq_ignore_ascii_case("fbx") {
            fbx_paths
                .entry(&item.id)
                .or_insert_with(|| vec![&item.file_path]);
        }
    }
    Ok(fbx_paths
        .into_iter()
        .map(|(id, paths)| {
            let path = exported_fbx(paths).expect("a group with an FBX");
            (id.to_string(), path.to_string())
        })
        .collect())
}

/// A pack folder of a library, with your edits.
#[derive(Debug, Clone)]
struct PackFolder {
    path: String,
    name: String,
    /// The `pack_json` the info is read from, for the fingerprint
    json: Option<String>,
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
    /// The FBX files read for their facts, stamped, which the check's `Walker` goes on from
    probes: Probes,
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
    /// The same files with their indexed sizes and mtimes, as the fingerprint holds them
    pack_listings: HashMap<usize, String>,
    /// Your edits (C4) to the items and to the other sources of their texture sets
    edits: Vec<edits::Row>,
    memo: hashes::Memo,
}

/// One message per general problem, before any asset is read.
fn general(message: impl Into<String>) -> Problem {
    Problem {
        message: message.into(),
        assets: Vec::new(),
    }
}

/// Checks the domain collection `domain_id`, reading FBX files' facts with `facts` and the files
/// inference reads through `memo`, without staging anything.
#[cfg(test)]
pub async fn check(
    db: &Database,
    domain_id: &str,
    facts: &impl fbx_facts::Reader,
    memo: &Arc<infer::Memo>,
) -> Result<CheckReport, String> {
    inspect(db, domain_id, facts, memo).await.map(|i| i.report)
}

/// Reads the domain collection `domain_id` as a Check does, reading FBX files' facts with
/// `facts` and the files inference reads through `memo`. An unknown collection is an error;
/// everything else that would refuse the export is a problem in the report.
pub async fn inspect(
    db: &Database,
    domain_id: &str,
    facts: &impl fbx_facts::Reader,
    memo: &Arc<infer::Memo>,
) -> Result<Inspection, String> {
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
    let mut fingerprint = Fingerprint::default();
    fingerprint.put(
        "",
        "the export settings",
        serde_json::to_string(&settings).expect("settings serialize"),
    );
    fingerprint.put("", "the domain's name", &domain);

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
            fingerprint.put("", "the root collection's name", name);
            if domain_parent.as_deref() != Some(id) {
                problems.push(general(format!(
                    "{domain} is not a domain: domains are the collections directly inside the root collection {name}"
                )));
            } else if collections
                .iter()
                .any(|c| c.0 != domain_id && c.2.as_deref() == Some(id) && c.1 == domain)
            {
                problems.push(general(format!(
                    "Another domain in {name} is also named {domain}: each domain is a folder of the bundle path, so rename one"
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
    // Every problem so far is about the bundle path: with one, no folder is touched
    let path_ok = problems.is_empty();

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
    let tree: Vec<_> = subtree.iter().filter_map(|id| find(id)).collect();
    fingerprint.put("", "the domain's subcollections", format!("{tree:?}"));
    for id in &subtree {
        if let Some((_, name, _, true)) = find(id) {
            problems.push(general(format!(
                "{name} is a smart collection: a domain and its subcollections must be ordinary collections"
            )));
        }
    }

    let bundle_path = match (&settings.root, &root_name) {
        (Some(root), Some(root_name)) if path_ok => Some(
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
            read_library(library, Members::Collections(&subtree), facts)
                .await
                .map_err(|e| e.to_string())?,
        );
    }
    for input in &inputs {
        input_fingerprint(input, &mut fingerprint);
    }
    let domain_name = domain.clone();
    let memo = Arc::clone(memo);
    let (evaluated, inputs) = tokio::task::spawn_blocking(move || {
        let evaluated = evaluate(&domain_name, &mut inputs, &memo);
        (evaluated, inputs)
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

    let Evaluated {
        assets,
        names,
        models,
        textures,
        packs,
        problems: mut found,
        manifest,
        files,
        probes,
    } = evaluated;
    for (input, probes) in inputs.iter().zip(probes) {
        let library = &input.library;
        for (path, stamp) in probes.seen {
            fingerprint.put(&library.id, format!("{path} in {}", library.name), stamp);
        }
    }
    problems.append(&mut found);
    let assets = with_status(assets, &problems);
    Ok(Inspection {
        report: CheckReport {
            domain,
            bundle_path,
            assets,
            packs,
            problems,
            manifest,
            diff: None,
            staged: false,
        },
        files,
        fingerprint,
        names,
        models,
        textures,
    })
}

/// Reads asset `asset_id` of `library` on its own, as a Check of a domain holding only it would,
/// for the Game tab of an asset in no domain: its name, materials and texture set. Nothing is
/// staged, and its problems are no domain's.
pub async fn inspect_alone(
    library: LibraryHandle,
    asset_id: &str,
    facts: &impl fbx_facts::Reader,
    memo: &Arc<infer::Memo>,
) -> Result<Inspection, String> {
    let mut inputs = vec![read_library(library, Members::Item(asset_id), facts)
        .await
        .map_err(|e| e.to_string())?];
    let memo = Arc::clone(memo);
    // No domain name: the manifest's errors about the domain are left out
    let evaluated = tokio::task::spawn_blocking(move || evaluate("", &mut inputs, &memo))
        .await
        .map_err(|e| e.to_string())?;
    Ok(Inspection {
        report: CheckReport {
            domain: String::new(),
            bundle_path: None,
            assets: evaluated.assets,
            packs: evaluated.packs,
            problems: evaluated.problems,
            manifest: evaluated.manifest,
            diff: None,
            staged: false,
        },
        files: evaluated.files,
        fingerprint: Fingerprint::default(),
        names: evaluated.names,
        models: evaluated.models,
        textures: evaluated.textures,
    })
}

/// Puts what a check read from one library's DB in `fingerprint`: the domain's items, which
/// folders are packs, and each pack of an item with your edits and its indexed files.
fn input_fingerprint(input: &LibraryInput, fingerprint: &mut Fingerprint) {
    let library = &input.library;
    let scope = library.id.as_str();
    let items: Vec<_> = input
        .items
        .iter()
        .map(|i| (&i.id, &i.file_path, &i.kind))
        .collect();
    fingerprint.put(
        scope,
        format!("the domain's members in {}", library.name),
        format!("{items:?}"),
    );
    // Each item's exported FBX, whose file is stamped in the probes
    let mut groups: Vec<(&String, &String)> =
        input.fbx.iter().map(|(id, f)| (id, &f.path)).collect();
    groups.sort();
    fingerprint.put(
        scope,
        format!("the domain's format groups in {}", library.name),
        format!("{groups:?}"),
    );
    let paths: Vec<&str> = input.packs.iter().map(|p| p.path.as_str()).collect();
    fingerprint.put(
        scope,
        format!("the packs of {}", library.name),
        format!("{paths:?}"),
    );
    for (p, listing) in &input.pack_listings {
        let pack = &input.packs[*p];
        let label = |what: &str| format!("{what} {} in {}", pack.path, library.name);
        fingerprint.put(
            scope,
            label("the pack"),
            format!("{:?} {:?} {:?}", pack.name, pack.json, pack.edits),
        );
        fingerprint.put(scope, label("the indexed files of the pack"), listing);
    }
    let edits: Vec<_> = input
        .edits
        .iter()
        .map(|e| (&e.file_path, &e.edits))
        .collect();
    fingerprint.put(
        scope,
        format!("your export edits in {}", library.name),
        format!("{edits:?}"),
    );
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

/// The pack folder among `packs` that the file at `path` is in, as a Check assigns an item its
/// pack: the innermost at or above its folder.
pub(crate) fn pack_at<'a>(packs: &'a [String], path: &str) -> Option<&'a str> {
    let by_path: HashMap<&str, usize> = packs
        .iter()
        .enumerate()
        .map(|(i, p)| (p.as_str(), i))
        .collect();
    innermost_pack(&by_path, dir_of(path)).map(|i| packs[i].as_str())
}

/// An indexed file of a pack: its id, path, kind, and indexed size and mtime.
type PackFile = (String, String, String, i64, i64);

/// The indexed files of pack `pack`, at `path` among `packs`, by path with their indexed sizes
/// and mtimes, without the files of the packs nested in it, which are their own.
async fn files_of_pack(
    pool: &sqlx::Pool<sqlx::Sqlite>,
    packs: &HashMap<&str, usize>,
    pack: usize,
    path: &str,
) -> sqlx::Result<Vec<PackFile>> {
    let below: Vec<PackFile> = sqlx::query_as(
        "SELECT id, file_path, kind, size_bytes, modified_at FROM assets
         WHERE ?1 = '' OR (file_path >= ?1 || '/' AND file_path < ?1 || '0')
         ORDER BY file_path",
    )
    .bind(path)
    .fetch_all(pool)
    .await?;
    Ok(below
        .into_iter()
        .filter(|(_, f, ..)| innermost_pack(packs, dir_of(f)) == Some(pack))
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
        .map(|(_, f, ..)| f)
        .collect();
    let root = library.root_path.clone();
    tokio::task::spawn_blocking(move || Some(packs::infer_origin(&files, &mut Walker::new(&root))))
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

/// What a check reads as its items.
enum Members<'a> {
    /// The grid items in these collections
    Collections(&'a [String]),
    /// One asset's grid item
    Item(&'a str),
}

async fn read_library(
    library: LibraryHandle,
    members: Members<'_>,
    facts: &impl fbx_facts::Reader,
) -> sqlx::Result<LibraryInput> {
    let pool = &library.pool;
    // Memberships are on every member of a group, so each is read as its grid item
    let items: Vec<ItemRow> = match members {
        Members::Collections(subtree) => {
            sqlx::query_as(
                "SELECT a.id, a.file_path, a.file_name, a.extension, a.kind FROM assets a
                 WHERE a.id IN (
                     SELECT COALESCE(m.group_id, m.id) FROM collection_assets ca
                     JOIN assets m ON m.id = ca.asset_id
                     WHERE ca.collection_id IN (SELECT value FROM json_each(?)))
                 ORDER BY a.file_path",
            )
            .bind(serde_json::to_string(subtree).expect("ids serialize"))
            .fetch_all(pool)
            .await?
        }
        Members::Item(id) => {
            sqlx::query_as(
                "SELECT id, file_path, file_name, extension, kind FROM assets
                 WHERE id = (SELECT COALESCE(group_id, id) FROM assets WHERE id = ?)",
            )
            .bind(id)
            .fetch_all(pool)
            .await?
        }
    };
    // Every read is sent at once, so the preview queue serves them all between two of its jobs
    let reads = exported_fbx_of(pool, &items)
        .await?
        .into_iter()
        .map(|(id, path)| {
            let root = &library.root_path;
            async move {
                // Stamped as it is read, for the fingerprint
                let source = root.join(&path);
                let stamp = Stamp::of(&source);
                let facts = facts.read(&source, root).await;
                (id, ItemFbx { path, facts }, stamp)
            }
        });
    let mut probes = Probes::new(&library.root_path);
    let mut fbx: HashMap<String, ItemFbx> = HashMap::new();
    for (id, item, stamp) in futures_util::future::join_all(reads).await {
        probes.record(&item.path, stamp);
        fbx.insert(id, item);
    }

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
            json,
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
    let mut pack_listings = HashMap::new();
    let mut pack_images = HashMap::new();
    for pack in item_packs.iter().flatten().collect::<HashSet<_>>() {
        let own = files_of_pack(pool, &by_path, *pack, &packs[*pack].path).await?;
        let listing: Vec<(&String, &i64, &i64)> = own
            .iter()
            .map(|(_, f, _, size, mtime)| (f, size, mtime))
            .collect();
        pack_listings.insert(*pack, format!("{listing:?}"));
        let images: Vec<infer::Image> = own
            .iter()
            .filter(|(_, _, kind, ..)| is_image(kind))
            .map(|(id, path, ..)| infer::Image {
                id: id.clone(),
                path: path.clone(),
            })
            .collect();
        pack_images.insert(*pack, images);
        pack_files.insert(*pack, own.into_iter().map(|(_, f, ..)| f).collect());
    }

    // Your edits to the items, and to every image of their packs, which may join as a texture
    let read: HashSet<&str> = items
        .iter()
        .map(|i| i.id.as_str())
        .chain(pack_images.values().flatten().map(|i| i.id.as_str()))
        .collect();
    let edits = edits::read_all(pool)
        .await?
        .into_iter()
        .filter(|row| read.contains(row.asset_id.as_str()))
        .collect();

    // Textures join through materials and sets, so every image of a pack may be hashed
    let paths: Vec<String> = items
        .iter()
        .map(|i| i.file_path.clone())
        .chain(pack_images.values().flatten().map(|i| i.path.clone()))
        .collect();
    let memo = hashes::load(pool, &paths).await?;
    Ok(LibraryInput {
        edits,
        fbx,
        probes,
        library,
        items,
        item_packs,
        packs,
        pack_files,
        pack_listings,
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
    /// The file to copy into the bundle
    source: PathBuf,
}

/// One asset as the check reads it, before names are compared.
struct Read {
    row: CheckedAsset,
    /// Index of its pack in the domain's packs
    pack: Option<usize>,
    ready: Option<ReadyTexture>,
}

/// What `evaluate` found, with the files each library's check stats, in library order.
struct Evaluated {
    assets: Vec<CheckedAsset>,
    names: HashMap<String, Field<String>>,
    models: HashMap<String, ModelGame>,
    textures: HashMap<String, TextureGame>,
    packs: Vec<DomainPack>,
    problems: Vec<Problem>,
    manifest: Value,
    files: Vec<BundleFile>,
    probes: Vec<Probes>,
}

/// A pack of a library, by the library's and the pack's indices in `inputs`.
type PackKey = (usize, usize);

/// The pack index of a model in no pack, whose catalog of images is empty.
const NO_PACK: usize = usize::MAX;

/// One image the domain exports as a texture: a member, or a source joined through a material
/// or its texture set (C3).
struct TextureJob {
    library: usize,
    asset: AssetRef,
    extension: String,
    /// Index of its pack in the domain's packs
    pack: Option<usize>,
    name: Field<String>,
    layout: Layout,
    convention: Convention,
    member: bool,
    game: TextureGame,
}

/// Everything a check does with files, run on a blocking thread: reads each item, its pack and
/// its file, infers the models' materials and the texture sets, compares names, and builds and
/// validates the manifest.
fn evaluate(domain: &str, inputs: &mut [LibraryInput], memo: &infer::Memo) -> Evaluated {
    let mut problems = Vec::new();
    let mut reads: Vec<Read> = Vec::new();
    let mut asset_names: HashMap<String, Field<String>> = HashMap::new();
    // The domain's packs in manifest order (library order, then path), by (library, pack index)
    let mut pack_order: Vec<PackKey> = Vec::new();
    for (l, input) in inputs.iter().enumerate() {
        let mut used: Vec<usize> = input.item_packs.iter().flatten().copied().collect();
        used.sort_by(|a, b| input.packs[*a].path.cmp(&input.packs[*b].path));
        used.dedup();
        pack_order.extend(used.into_iter().map(|p| (l, p)));
    }
    let mut walkers: Vec<Walker> = inputs
        .iter_mut()
        .map(|i| {
            Walker::with_probes(std::mem::replace(
                &mut i.probes,
                Probes::new(&i.library.root_path),
            ))
        })
        .collect();
    let sources: Vec<PackSource> = pack_order
        .iter()
        .map(|(l, p)| {
            let input = &inputs[*l];
            let pack = &input.packs[*p];
            let files = input.pack_files.get(p).map_or(&[][..], Vec::as_slice);
            let walker = &mut walkers[*l];
            PackSource {
                library_id: input.library.id.clone(),
                path: pack.path.clone(),
                folder_name: pack.name.clone(),
                package_name: pack
                    .info
                    .package
                    .as_ref()
                    .and_then(|f| packs::read_package_name(&mut walker.probes, &f.path)),
                origin: pack
                    .edits
                    .origin
                    .unwrap_or_else(|| packs::infer_origin(files, walker)),
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

    // Your edits to each source image (C4), by library and asset id
    let source_edits: Vec<HashMap<String, SourceEdits>> = inputs
        .iter()
        .map(|input| {
            (input.edits.iter())
                .map(|row| (row.asset_id.clone(), edits::source_edits(&row.parsed())))
                .collect()
        })
        .collect();
    // Each pack's images in their texture sets (C8), and what a Unity pack's files say
    let mut catalogs: HashMap<PackKey, PackImages> = HashMap::new();
    for (l, input) in inputs.iter_mut().enumerate() {
        for (p, images) in input.pack_images.drain() {
            catalogs.insert((l, p), PackImages::new(images).with_edits(&source_edits[l]));
        }
    }
    let mut unity_packs: HashMap<PackKey, UnityPack> = HashMap::new();

    // Each model with materials, as (its index in `reads`, its pack, its library and its item),
    // and each member image in a pack, as (its pack, its index in the pack's images)
    let mut models: Vec<(usize, PackKey, usize, usize)> = Vec::new();
    let mut members: Vec<(PackKey, usize)> = Vec::new();
    let mut jobs: Vec<TextureJob> = Vec::new();
    let mut seen = HashSet::new();
    for (l, input) in inputs.iter().enumerate() {
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
            let fbx = input.fbx.get(&item.id);
            let row = CheckedAsset {
                asset: asset.clone(),
                kind: exported_kind(item, fbx.map(|f| &f.facts)),
                asset_name: asset_name(item, &input.edits).map(|name| name.value()),
                role: None,
                pack: pack.map(|p| export_packs[p].id.clone()),
                status: Status::Ready,
                member: true,
                layout: None,
                normal_convention: None,
                materials: Vec::new(),
            };
            match AssetKind::from_str(&item.kind) {
                AssetKind::Model3d => match fbx {
                    None => {
                        issue("it has no FBX file: a model exports from its group's FBX".into())
                    }
                    Some(fbx) => {
                        read_fbx(fbx, &item.file_path, &mut issue);
                        if let Ok(facts) = &fbx.facts {
                            if facts.reading() == Some(Reading::Model) {
                                if let (Some(key), Some(p)) = (key, pack) {
                                    if sources[p].origin == Origin::Unity {
                                        unity_packs.entry(key).or_insert_with(|| {
                                            UnityPack::read(
                                                &input.pack_files[&key.1],
                                                &mut walkers[l].probes,
                                                memo,
                                            )
                                        });
                                    }
                                }
                                // A model in no pack still has its slots, the slot the export
                                // adds and their names checked, with no image to bind
                                let key = key.unwrap_or((l, NO_PACK));
                                catalogs
                                    .entry(key)
                                    .or_insert_with(|| PackImages::new(Vec::new()));
                                models.push((reads.len(), key, l, i));
                            }
                        }
                    }
                },
                AssetKind::Texture | AssetKind::Image | AssetKind::Hdri => {
                    let found = key.and_then(|key| {
                        let image = catalogs.get(&key)?.find(&item.file_path)?;
                        Some((key, image))
                    });
                    match found {
                        Some(member) => members.push(member),
                        // An image in no pack has no set: it is a texture on its own
                        None => {
                            let stem_info = textures::read_stem(item.stem());
                            let inferred =
                                stem_info.layout.unwrap_or(Layout::Role(Role::BaseColor));
                            let convention = stem_info.convention.unwrap_or(Convention::OpenGl);
                            let edits = source_edits[l].get(&item.id).copied().unwrap_or_default();
                            let layout = edits.layout(inferred);
                            let alone = PackImage {
                                asset_id: item.id.clone(),
                                file_path: item.file_path.clone(),
                                layout: layout.label(),
                                roles: layout.outputs().to_vec(),
                            };
                            jobs.push(TextureJob {
                                library: l,
                                asset,
                                extension: item.extension.to_ascii_lowercase(),
                                pack,
                                name: asset_name(item, &input.edits).expect("an image's name"),
                                layout,
                                convention: edits.convention.unwrap_or(convention),
                                member: true,
                                game: TextureGame::new(inferred, convention, edits, vec![alone]),
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
            // Images went on as texture jobs, which record theirs
            if let Some(name) = asset_name(item, &input.edits) {
                asset_names.insert(item.id.clone(), name);
            }
            reads.push(Read {
                row,
                pack,
                ready: None,
            });
        }
    }

    // The models' materials, inferred over the whole domain (C7, D24)
    let domain_models: Vec<infer::DomainModel<PackKey>> = models
        .iter()
        .map(|(r, key, l, i)| {
            let input = &inputs[*l];
            let item = &input.items[*i];
            let fbx = &input.fbx[&item.id];
            infer::DomainModel {
                pack: *key,
                library: *l,
                unity: unity_packs.get(key),
                item_path: &item.file_path,
                fbx_path: &fbx.path,
                facts: fbx.facts.as_ref().expect("a model's facts"),
                asset_name: reads[*r].row.asset_name.clone().unwrap_or_default(),
                edits: (input.edits.iter())
                    .find(|e| e.asset_id == item.id)
                    .map(|e| edits::slot_edits(&e.parsed()))
                    .unwrap_or_default(),
            }
        })
        .collect();
    let mut probes: Vec<&mut Probes> = walkers.iter_mut().map(|w| &mut w.probes).collect();
    let (materials, set_names) = infer::domain(&mut catalogs, &domain_models, &mut probes, memo);
    // Your name for a set (C4): the first of its sources by path that holds one
    let yours = |key: PackKey, set: usize| {
        let images = &catalogs[&key];
        let ids: HashSet<&str> = images.sets[set]
            .sources
            .iter()
            .map(|i| images.images[*i].id.as_str())
            .collect();
        edits::first_name(
            inputs[key.0]
                .edits
                .iter()
                .filter(|e| ids.contains(e.asset_id.as_str())),
        )
    };
    let mut model_games: HashMap<String, ModelGame> = HashMap::new();
    for ((r, key, ..), mut materials) in models.iter().zip(materials) {
        let bindings = (materials.iter_mut()).flat_map(|m| {
            m.textures
                .values_mut()
                .chain(m.inferred.textures.values_mut())
        });
        for binding in bindings {
            if let Some(name) = yours(*key, binding.set) {
                binding.asset_name = name;
            }
        }
        let row = &mut reads[*r].row;
        let asset = row.asset.clone();
        let mut issue = |message: String| {
            problems.push(Problem {
                message: format!("{}: {message}", asset.file_path),
                assets: vec![asset.clone()],
            })
        };
        material_names(&materials, &mut issue);
        for message in materials.iter().flat_map(|m| &m.unfilled) {
            issue(message.clone());
        }
        let images = &catalogs[key];
        model_games.insert(
            asset.asset_id.clone(),
            ModelGame {
                materials: materials.clone(),
                images: (0..images.images.len())
                    .map(|i| PackImage::of(images, i))
                    .collect(),
            },
        );
        row.materials = materials;
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
    for (r, key, ..) in &models {
        let images = &catalogs[key];
        for material in &reads[*r].row.materials {
            for (role, binding) in &material.textures {
                let sources = images.role_sources(binding.set, *role);
                wanted.extend(sources.into_iter().map(|i| (*key, i)));
            }
        }
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
                .to_ascii_lowercase(),
            pack: Some(pack),
            name: Field {
                inferred: set_names.name(&catalogs, key, images.set_of[image]),
                yours: yours(key, images.set_of[image]),
            },
            layout: images.layout(image),
            convention: images.convention(image, sources[pack].normal_convention()),
            member: member_images.contains(&(key, image)),
            game: TextureGame::new(
                images.inferred_layout(image),
                images.inferred_convention(image, sources[pack].normal_convention()),
                images.edits(image),
                (images.sets[images.set_of[image]].sources.iter())
                    .map(|i| PackImage::of(images, *i))
                    .collect(),
            ),
        });
    }

    // Each invalid conversion.json, by library and path, with the assets it covers
    let mut invalid_conversions: BTreeMap<(usize, String), (Invalid, Vec<AssetRef>)> =
        BTreeMap::new();
    let mut texture_games: HashMap<String, TextureGame> = HashMap::new();
    for job in jobs {
        texture_games.insert(job.asset.asset_id.clone(), job.game.clone());
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
            asset_name: Some(job.name.value()),
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
            (Ok(pass), Some(pack), Some(conversion)) if is_pascal(&job.name.value()) => {
                match input.memo.sha256(&mut walkers[job.library].probes, path) {
                    Ok(sha256) => {
                        ready = Some(ReadyTexture {
                            name: job.name.value(),
                            role: pass.role,
                            extension: pass.extension,
                            sha256,
                            source_path: relative_to(&sources[pack].path, path),
                            conversion,
                            source: input.library.root_path.join(path),
                        })
                    }
                    Err(e) => issue(format!("it can't be read: {e}")),
                }
            }
            _ => {}
        }
        asset_names.insert(job.asset.asset_id.clone(), job.name.clone());
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
    let (manifest, mut invalid, files) = manifest(domain, &reads, &export_packs, &problems);
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
    Evaluated {
        assets: rows,
        packs: domain_packs,
        problems,
        manifest,
        files,
        probes: walkers.into_iter().map(|w| w.probes).collect(),
        names: asset_names,
        models: model_games,
        textures: texture_games,
    }
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

/// What refuses a model item exporting from `fbx` (C3): an FBX that can't be read, has two
/// materials of one name, or has no kept mesh and no take, and the build-order guards on models
/// and animations. `exported_kind` says which of the two it is.
fn read_fbx(fbx: &ItemFbx, item_path: &str, issue: &mut impl FnMut(String)) {
    let what = if fbx.path == item_path {
        "FBX".to_string()
    } else {
        format!("its FBX {}", fbx.path)
    };
    let facts = match &fbx.facts {
        Ok(facts) => facts,
        Err(e) => {
            issue(format!("{what} can't be read for export: {e}"));
            return;
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
            issue("model export not built yet (an FBX with a kept mesh exports as a model)".into())
        }
        Some(Reading::Animation) => issue("animation export not built yet (an FBX with no kept mesh and a take exports as an animation)".into()),
        None => issue(format!("{what} has no kept mesh and no take")),
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
                "{} {what}: {}. Give one another name in its Game tab",
                rows.len(),
                paths.join(" and ")
            ),
            assets: rows.iter().map(|r| r.asset.clone()).collect(),
        });
    }
}

/// One asset's manifest entry, with the asset it is, its pack's index in the domain's packs and
/// the file to copy into the bundle.
struct Entry<'a> {
    value: Value,
    asset: &'a AssetRef,
    pack: usize,
    source: PathBuf,
}

/// The manifest of the assets no problem names, the problems its schema validation and the
/// importer's rules (C1) find, each naming the asset at its pointer, and the manifest's files.
fn manifest(
    domain: &str,
    reads: &[Read],
    export_packs: &[packs::ExportPack],
    problems: &[Problem],
) -> (Value, Vec<Problem>, Vec<BundleFile>) {
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
            source: texture.source.clone(),
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
/// Returns that manifest, the problems found on the way and the manifest's files. `exportedAt` is
/// the local time with its offset (RFC 3339, C15).
fn validated(
    domain: &str,
    mut entries: Vec<Entry>,
    export_packs: &[packs::ExportPack],
) -> (Value, Vec<Problem>, Vec<BundleFile>) {
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
            let files = entries
                .into_iter()
                .map(|e| BundleFile {
                    file: e.value["file"].as_str().unwrap_or_default().to_string(),
                    sha256: e.value["sha256"].as_str().unwrap_or_default().to_string(),
                    source: e.source,
                    asset: e.asset.clone(),
                })
                .collect();
            return (manifest, found, files);
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

/// `assets` with the status their problems give them: `Problem` for each asset a problem names.
/// The only place that rule lives; `bundle` reapplies it after adding its own problems, and only
/// refines the other assets' status by the diff.
pub fn with_status(mut assets: Vec<CheckedAsset>, problems: &[Problem]) -> Vec<CheckedAsset> {
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
    use crate::export::infer::{Surface, Via};
    use crate::export::test_fixture::*;
    use serde_json::json;

    /// A fresh inference memo, so no test reads another's answers.
    fn fresh_memo() -> Arc<infer::Memo> {
        Arc::new(infer::Memo::default())
    }

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

    /// Checks a new domain holding `paths`.
    async fn check_domain(f: &Fixture, paths: &[&str]) -> CheckReport {
        collection(f, "domain", "Props", "dg", paths).await;
        check(&f.db, "domain", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap()
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
        let again = check(&f.db, "domain", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap();
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
        let again = check(&f.db, "domain", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap();
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
        let report = check(&f.db, "domain", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap();

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
            ["2 textures are named Banner with the role BaseColor: Dungeon Props/Textures/T_Banner_BaseColor.png and Kenney Crates/banner.png. Give one another name in its Game tab"]
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

    fn name_edit(name: &str) -> edits::Edits {
        json!({ "assetName": name }).as_object().unwrap().clone()
    }

    /// AC3: an edit fixes a name collision, and keeps fixing it once the file is reindexed.
    #[tokio::test]
    async fn an_edit_fixes_a_name_collision_and_survives_a_reindex() {
        let f = fixture().await;
        let banner = "Kenney Crates/banner.png";
        let report = check_domain(
            &f,
            &["Dungeon Props/Textures/T_Banner_BaseColor.png", banner],
        )
        .await;
        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.problems[0].assets.len(), 2);

        let id = id_of(&f.library.pool, banner).await;
        edits::write(&f.library.pool, &id, &name_edit("KenneyBanner"))
            .await
            .unwrap();
        // The file changes and is indexed again, keeping its id and so its edit
        std::fs::write(f.library.root_path.join(banner), fixture_bytes(banner)).unwrap();
        let root = f.library.root_path.display().to_string();
        crate::watcher::index_paths(&f.db, "fixture", &[root])
            .await
            .unwrap();
        assert_eq!(id_of(&f.library.pool, banner).await, id);

        let report = check(&f.db, "domain", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap();
        assert_eq!(messages(&report), Vec::<&str>::new());
        assert_valid(&report);
        let names: Vec<(String, String)> = texture_entries(&report)
            .into_iter()
            .map(|(name, _, file)| (name, file))
            .collect();
        assert_eq!(
            names,
            [
                ("Banner", "Textures/Banner_BaseColor.png"),
                ("KenneyBanner", "Textures/KenneyBanner_BaseColor.png"),
            ]
            .map(|(n, p)| (n.to_string(), p.to_string()))
        );

        // A reset gives the collision back
        edits::write(
            &f.library.pool,
            &id,
            json!({ "assetName": null }).as_object().unwrap(),
        )
        .await
        .unwrap();
        let report = check(&f.db, "domain", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap();
        assert_eq!(report.problems.len(), 1);
    }

    #[tokio::test]
    async fn a_name_written_to_any_member_names_the_group_and_its_texture_set() {
        let f = fixture().await;
        let pool = &f.library.pool;
        let obj = id_of(pool, "Dungeon Props/Models/SM_Chest.obj").await;
        edits::write(pool, &obj, &name_edit("TreasureChest"))
            .await
            .unwrap();
        // The set's Normal is no member, but its name is the set's
        let normal = id_of(pool, "Dungeon Props/Textures/T_StoneFloor_N.png").await;
        edits::write(pool, &normal, &name_edit("Flagstones"))
            .await
            .unwrap();

        let report = check_domain(
            &f,
            &[
                "Dungeon Props/Models/SM_Chest.fbx",
                "Dungeon Props/Textures/T_StoneFloor_D.png",
            ],
        )
        .await;
        let names: Vec<(&str, Option<&str>)> = report
            .assets
            .iter()
            .map(|a| (a.asset.file_path.as_str(), a.asset_name.as_deref()))
            .collect();
        assert_eq!(
            names,
            [
                ("Dungeon Props/Models/SM_Chest.fbx", Some("TreasureChest")),
                (
                    "Dungeon Props/Textures/T_StoneFloor_D.png",
                    Some("Flagstones")
                ),
                // Brought in with its set, under the set's name
                (
                    "Dungeon Props/Textures/T_StoneFloor_N.png",
                    Some("Flagstones")
                ),
            ]
        );
    }

    /// A set a model uses is named after the model (D24) until you name it, and your name
    /// reaches the model's binding too.
    #[tokio::test]
    async fn your_set_name_wins_over_the_name_its_model_gives_it() {
        let f = fixture().await;
        let model = "Dungeon Props/Models/SM_Crate.fbx";
        let normal = "Dungeon Props/Textures/1K/T_Crate_N.png";
        let binding = |report: &CheckReport| {
            let crate_ = report
                .assets
                .iter()
                .find(|a| a.asset.file_path == model)
                .unwrap();
            crate_.materials[0].textures[&Role::Normal]
                .asset_name
                .clone()
        };
        let texture = |report: &CheckReport| {
            report
                .assets
                .iter()
                .find(|a| a.asset.file_path == normal)
                .and_then(|a| a.asset_name.clone())
        };
        let report = check_domain(&f, &[model]).await;
        assert_eq!(binding(&report), "Crate");
        assert_eq!(texture(&report).as_deref(), Some("Crate"));

        let id = id_of(&f.library.pool, normal).await;
        edits::write(&f.library.pool, &id, &name_edit("CrateBoards"))
            .await
            .unwrap();
        let report = check(&f.db, "domain", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap();
        assert_eq!(binding(&report), "CrateBoards");
        assert_eq!(texture(&report).as_deref(), Some("CrateBoards"));
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

        let report = check(&f.db, "lower", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap();
        assert_eq!(
            messages(&report),
            [
                "The domain's name props isn't PascalCase (a capital letter, then letters and digits): it is a folder of the bundle path and the manifest's domain",
                "Recent is a smart collection: a domain and its subcollections must be ordinary collections",
            ]
        );

        let report = check(&f.db, "nested", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap();
        assert_eq!(
            messages(&report),
            ["Nested is not a domain: domains are the collections directly inside the root collection DungeonGame"]
        );

        sqlx::query("UPDATE collections SET name = 'Dungeon Game' WHERE id = 'dg'")
            .execute(&f.db.master_pool)
            .await
            .unwrap();
        collection(&f, "props", "Props", "dg", &[]).await;
        let report = check(&f.db, "props", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap();
        assert_eq!(
            messages(&report),
            ["The root collection's name Dungeon Game isn't PascalCase (a capital letter, then letters and digits): it is a folder of the bundle path"]
        );

        assert!(check(&f.db, "missing", &Facts::fixture(), &fresh_memo())
            .await
            .is_err());
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

        let report = check(&f.db, "props", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap();
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
        let report = check(&f.db, "props", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap();
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
            &f.db,
            &second,
            "domain",
            &["Foo Copy/tile_c.png", "Unreal/Rocks/T_Rock_D.png"],
        )
        .await;
        let report = check(&f.db, "domain", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap();

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
    async fn an_upper_case_extension_passes_through_as_a_member_and_a_set_mate() {
        let f = fixture().await;
        let png = fixture_bytes("Dungeon Props/Textures/T_StoneFloor_D.png");
        let normal = fixture_bytes("Dungeon Props/Textures/T_StoneFloor_N.png");
        add_file(&f, "Kenney Crates/Tiles/T_Tile_D.PNG", &png).await;
        add_file(&f, "Kenney Crates/Tiles/T_Tile_N.PNG", &normal).await;
        let report = check_domain(&f, &["Kenney Crates/Tiles/T_Tile_D.PNG"]).await;

        assert_eq!(messages(&report), Vec::<&str>::new());
        let rows: Vec<(&str, bool, Status)> = report
            .assets
            .iter()
            .map(|a| (a.asset.file_path.as_str(), a.member, a.status))
            .collect();
        assert_eq!(
            rows,
            [
                ("Kenney Crates/Tiles/T_Tile_D.PNG", true, Status::Ready),
                ("Kenney Crates/Tiles/T_Tile_N.PNG", false, Status::Ready),
            ]
        );
        let entries = texture_entries(&report);
        let expected = [
            ("Tile", "BaseColor", "Textures/Tile_BaseColor.png"),
            ("Tile", "Normal", "Textures/Tile_Normal.png"),
        ];
        assert_eq!(
            entries,
            expected.map(|(n, r, p)| (n.to_string(), r.to_string(), p.to_string()))
        );
        assert_valid(&report);
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

        let report = check(&f.db, "props", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap();
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
            source: PathBuf::from(format!("/library/{name}.png")),
        };
        let entries = vec![
            entry("Moss", &"0".repeat(64), 1, &moss),
            // A bad sha256: the schema refuses this asset
            entry("Rock", "BAD", 0, &rock),
        ];

        let (manifest, problems, files) =
            validated("Props", entries, &[pack("rocks"), pack("moss")]);

        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].assets, std::slice::from_ref(&rock));
        assert!(problems[0]
            .message
            .starts_with("Pack/rock.png: the manifest is invalid at /assets/1/sha256: "));
        assert_eq!(manifest["assets"].as_array().unwrap().len(), 1);
        assert_eq!(manifest["assets"][0]["assetName"], "Moss");
        let files: Vec<(&str, &Path)> = files
            .iter()
            .map(|f| (f.file.as_str(), f.source.as_path()))
            .collect();
        assert_eq!(
            files,
            [(
                "Textures/Moss_BaseColor.png",
                Path::new("/library/Moss.png")
            )]
        );
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
        let report = check(&f.db, "domain", &facts, &fresh_memo()).await.unwrap();

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
        let report = check(&f.db, "domain", &facts, &fresh_memo()).await.unwrap();

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
            check(&f.db, "domain", &facts, &fresh_memo()),
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
        check(&f.db, "domain", facts, &fresh_memo()).await.unwrap()
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
        // Every binding names a file that gives its role
        for asset in &report.assets {
            for material in &asset.materials {
                for binding in material.textures.values() {
                    assert!(!binding.files.is_empty(), "{}", asset.asset.file_path);
                }
            }
        }

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
    async fn an_image_two_models_disagree_about_has_one_layout_for_both() {
        let f = fixture().await;
        // SM_Leaf's FBX calls T_LeafAtlas, which has no suffix, a normal map, and SM_Log's
        // Leaves.mat a base color. The Unity property wins, whichever model is read first, and
        // SM_Leaf's slot takes the image as what it is
        let leaf = "Unity Moss/Models/SM_Leaf.fbx";
        add_file(&f, leaf, &fixture_bytes("Unity Moss/Models/SM_Log.fbx")).await;
        let facts = Facts::fixture().with(
            leaf,
            Ok(serde_json::from_value(json!({
                "slots": [{"name": "Frond", "textures": {"normal": [
                    {"file": null, "relative": "../Textures/T_LeafAtlas.png"}
                ]}}],
                "kept_meshes": ["SM_Leaf"], "dropped_meshes": [], "has_kept_mesh": true,
                "kept_mesh_without_material": false, "bones": [], "takes": [],
            }))
            .unwrap()),
        );
        let report = check_domain_with(&f, &[leaf, "Unity Moss/Models/SM_Log.fbx"], &facts).await;

        // Used by two models, the set keeps its stem (D24)
        assert_eq!(
            materials_of(&report, leaf)[0].5,
            [(Role::BaseColor, "LeafAtlas".to_string(), Via::Fbx)]
        );
        let leaves = &materials_of(&report, "Unity Moss/Models/SM_Log.fbx")[1];
        assert_eq!(
            leaves.5,
            [(Role::BaseColor, "LeafAtlas".to_string(), Via::Unity)]
        );
        for asset in &report.assets {
            for material in &asset.materials {
                for binding in material.textures.values() {
                    assert!(!binding.files.is_empty(), "{}", asset.asset.file_path);
                }
            }
        }
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
    async fn a_model_in_no_pack_still_has_its_slots_and_their_names_checked() {
        let f = fixture().await;
        let table = "Loose/SM_Table.fbx";
        add_file(
            &f,
            table,
            &fixture_bytes("Dungeon Props/Models/SM_Stool.fbx"),
        )
        .await;
        let facts = Facts::fixture().with(
            table,
            Ok(serde_json::from_value(json!({
                "slots": [{"name": "M_Wood"}, {"name": "Wood"}],
                "kept_meshes": ["SM_Table", "SM_Table_Top"], "dropped_meshes": [],
                "has_kept_mesh": true, "kept_mesh_without_material": true,
                "bones": [], "takes": [],
            }))
            .unwrap()),
        );
        let report = check_domain_with(&f, &[table], &facts).await;

        assert_eq!(
            messages(&report),
            [
                "Loose/SM_Table.fbx: it is in no pack: mark its folder as a pack",
                "Loose/SM_Table.fbx: model export not built yet (an FBX with a kept mesh exports as a model)",
                "Loose/SM_Table.fbx: 2 of its materials are named Wood: slots M_Wood and Wood. Rename one so each name is unique",
            ]
        );
        let slots: Vec<(String, bool)> = materials_of(&report, table)
            .into_iter()
            .map(|m| (m.0, m.4))
            .collect();
        assert_eq!(
            slots,
            [("M_Wood", false), ("Wood", false), ("OpenDAMDefault", true)]
                .map(|(s, added)| (s.to_string(), added))
        );
        let row = report
            .assets
            .iter()
            .find(|a| a.asset.file_path == table)
            .unwrap();
        assert_eq!(row.pack, None);
        assert!(row.materials.iter().all(|m| m.textures.is_empty()));
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

    /// An FBX read for its facts is in the fingerprint, by its size and mtime, so touching it
    /// changes the Check's inputs.
    #[tokio::test]
    async fn an_fbx_read_for_its_facts_is_stamped_in_the_fingerprint() {
        let f = fixture().await;
        let fbx = "Dungeon Props/Models/SM_Chest.fbx";
        collection(&f, "domain", "Props", "dg", &[fbx]).await;
        let facts = Facts::fixture();
        let before = inspect(&f.db, "domain", &facts, &fresh_memo())
            .await
            .unwrap();
        assert_eq!(facts.read_files(), [fbx]);
        let again = inspect(&f.db, "domain", &facts, &fresh_memo())
            .await
            .unwrap();
        assert_eq!(
            before.fingerprint.changes(&again.fingerprint),
            Vec::<String>::new()
        );

        let file = std::fs::File::options()
            .write(true)
            .open(f.library.root_path.join(fbx))
            .unwrap();
        let later =
            file.metadata().unwrap().modified().unwrap() + std::time::Duration::from_secs(3600);
        file.set_modified(later).unwrap();
        let after = inspect(&f.db, "domain", &facts, &fresh_memo())
            .await
            .unwrap();
        assert_eq!(
            before.fingerprint.changes(&after.fingerprint),
            [format!("{fbx} in Fixture")]
        );
    }

    /// The Unity files material inference reads aren't indexed, so they are in the fingerprint
    /// only through the Check's probes: touching one changes the Check's inputs.
    #[tokio::test]
    async fn the_unity_files_inference_reads_are_stamped_in_the_fingerprint() {
        let f = fixture().await;
        let log = "Unity Moss/Models/SM_Log.fbx";
        collection(&f, "domain", "Props", "dg", &[log]).await;
        let facts = Facts::fixture();
        let memo = fresh_memo();
        let before = inspect(&f.db, "domain", &facts, &memo).await.unwrap();
        for file in [
            "Unity Moss/Models/SM_Log.fbx.meta",
            "Unity Moss/Textures/T_LeafAtlas.png.meta",
        ] {
            let handle = std::fs::File::options()
                .write(true)
                .open(f.library.root_path.join(file))
                .unwrap();
            let later = handle.metadata().unwrap().modified().unwrap()
                + std::time::Duration::from_secs(3600);
            handle.set_modified(later).unwrap();
        }
        let after = inspect(&f.db, "domain", &facts, &memo).await.unwrap();
        assert_eq!(
            before.fingerprint.changes(&after.fingerprint),
            [
                "Unity Moss/Models/SM_Log.fbx.meta in Fixture",
                "Unity Moss/Textures/T_LeafAtlas.png.meta in Fixture",
            ]
        );
    }

    /// Writes your export edit `value` to the fixture's file at `path`.
    async fn edit(f: &Fixture, path: &str, value: Value) {
        let id = id_of(&f.library.pool, path).await;
        edits::write(&f.library.pool, &id, value.as_object().unwrap())
            .await
            .unwrap();
    }

    async fn recheck(f: &Fixture) -> CheckReport {
        check(&f.db, "domain", &Facts::fixture(), &fresh_memo())
            .await
            .unwrap()
    }

    /// AC4's edit part: each edit replaces its inferred value in what the export uses, and a
    /// reset restores the inferred one.
    #[tokio::test]
    async fn each_edit_replaces_its_inferred_value_and_a_reset_restores_it() {
        use Role::*;
        use Surface::*;
        use Via::*;
        let f = fixture().await;
        let (crate_, gate, guard) = (
            "Dungeon Props/Models/SM_Crate.fbx",
            "Dungeon Props/Models/SM_Gate.fbx",
            "Dungeon Props/Models/SK_Guard.fbx",
        );
        let (aorm, rock_n) = (
            "Dungeon Props/Textures/T_Guard_AORM.png",
            "Dungeon Props/Rocks/T_Rock_N_DX.png",
        );
        let inferred = check_domain(&f, &[crate_, gate, guard, rock_n]).await;
        assert_eq!(
            materials_of(&inferred, guard),
            [material(
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
            )]
        );

        let crate_4k = id_of(&f.library.pool, "Dungeon Props/Textures/4K/T_Crate_N.png").await;
        let planks = json!({"Planks": {"textures": {"Normal": crate_4k}}});
        edit(&f, crate_, json!({ "materials": planks })).await;
        let iron = json!({"Iron": {"name": "Metal", "textures": {"BaseColor": null}}});
        edit(&f, gate, json!({ "materials": iron })).await;
        // The cutout suggestion, accepted
        edit(
            &f,
            guard,
            json!({"materials": {"Cloth": {"surface": "cutout"}}}),
        )
        .await;
        // The AORM map passes its AO through as it is, and the DirectX map is OpenGL after all
        let occlusion = json!({"layout": "passThrough", "role": "Occlusion"});
        edit(&f, aorm, occlusion).await;
        edit(&f, rock_n, json!({"normalConvention": "opengl"})).await;
        let yours = recheck(&f).await;

        assert_eq!(
            materials_of(&yours, crate_),
            [material(
                "Planks",
                "Planks",
                Opaque,
                false,
                false,
                &[(Normal, "Crate", Yours)]
            )]
        );
        let planks = &yours
            .assets
            .iter()
            .find(|a| a.asset.file_path == crate_)
            .unwrap()
            .materials[0];
        assert_eq!(
            planks.textures[&Normal].files,
            ["Dungeon Props/Textures/4K/T_Crate_N.png"]
        );
        assert_eq!(planks.textures[&Normal].source, crate_4k);
        assert_eq!(
            planks.inferred.textures[&Normal].files,
            ["Dungeon Props/Textures/1K/T_Crate_N.png"],
            "what was inferred is kept beside your pick"
        );
        assert_eq!(
            materials_of(&yours, gate),
            [material(
                "Iron",
                "Metal",
                Opaque,
                false,
                false,
                &[(MetallicSmoothness, "Gate", Suffix)]
            )]
        );
        assert_eq!(
            materials_of(&yours, guard),
            [material(
                "Cloth",
                "Cloth",
                Cutout,
                false,
                false,
                &[(BaseColor, "Guard", Suffix), (Occlusion, "Guard", Suffix)],
            )]
        );

        use Status::*;
        let rows = texture_rows(&yours);
        let row = |path: &str| rows.iter().find(|r| r.0 == path).copied();
        // Only what the export now uses joins the domain
        assert!(row("Dungeon Props/Textures/4K/T_Crate_N.png").is_some());
        assert_eq!(row("Dungeon Props/Textures/1K/T_Crate_N.png"), None);
        assert_eq!(row("Dungeon Props/Textures/T_Iron_D.png"), None);
        assert_eq!(
            row(aorm),
            Some((aorm, "Guard", "Occlusion", None, false, Ready))
        );
        assert_eq!(
            row(rock_n),
            Some((
                rock_n,
                "Rock",
                "Normal",
                Some(Convention::OpenGl),
                true,
                Ready
            ))
        );
        let entries = texture_entries(&yours);
        for entry in [
            ("Crate", "Normal", "Textures/Crate_Normal.png"),
            ("Guard", "Occlusion", "Textures/Guard_Occlusion.png"),
            ("Rock", "Normal", "Textures/Rock_Normal.png"),
        ] {
            let entry = (entry.0.into(), entry.1.into(), entry.2.into());
            assert!(entries.contains(&entry), "{entry:?} in {entries:?}");
        }
        assert_valid(&yours);

        // Each reset gives back what was inferred, and leaves no row
        edit(&f, crate_, json!({"materials": null})).await;
        edit(&f, gate, json!({"materials": {"Iron": null}})).await;
        edit(&f, guard, json!({"materials": {"Cloth": {}}})).await;
        edit(&f, aorm, json!({"layout": null, "role": null})).await;
        edit(&f, rock_n, json!({"normalConvention": null})).await;
        let reset = recheck(&f).await;
        for path in [crate_, gate, guard] {
            assert_eq!(
                materials_of(&reset, path),
                materials_of(&inferred, path),
                "{path}"
            );
        }
        assert_eq!(texture_rows(&reset), texture_rows(&inferred));
        assert_eq!(edits::read_all(&f.library.pool).await.unwrap(), []);
    }

    #[tokio::test]
    async fn a_pick_that_gives_no_role_is_a_problem_and_a_fixed_layout_ignores_your_role() {
        let f = fixture().await;
        let gate = "Dungeon Props/Models/SM_Gate.fbx";
        let metallic = id_of(&f.library.pool, "Dungeon Props/Textures/T_Iron_M.png").await;
        let pick = json!({"Iron": {"textures": {"BaseColor": metallic}}});
        edit(&f, gate, json!({ "materials": pick })).await;
        // A Roughness map merges into its set's MetallicSmoothness, which fixes its role
        edit(
            &f,
            "Dungeon Props/Textures/T_Iron_R.png",
            json!({"role": "Height"}),
        )
        .await;
        let report = check_domain(&f, &[gate]).await;
        assert!(messages(&report).contains(
            &"Dungeon Props/Models/SM_Gate.fbx: its material Iron has Dungeon Props/Textures/T_Iron_M.png picked as its BaseColor, but that image's layout gives no BaseColor: change its layout in its Game tab, or pick another"
        ), "{:?}", messages(&report));
        let model = report
            .assets
            .iter()
            .find(|a| a.asset.file_path == gate)
            .unwrap();
        assert!(!model.materials[0].textures.contains_key(&Role::BaseColor));
        let rows = texture_rows(&report);
        let rough = rows.iter().find(|r| r.0.ends_with("T_Iron_R.png")).unwrap();
        assert_eq!(rough.2, "Roughness");
    }
}
