//! The Export view's Check (plan game-export C14): reads a domain, builds its manifest in memory,
//! validates it, and lists every asset with its kind, default `assetName`, pack and status, and
//! every problem that would refuse the export. It also returns what the bundle module needs to
//! stage the bundle: the files to copy, and the fingerprint of everything it read (C14).
//!
//! A domain is a direct child of the root collection (C3). Its assets are the grid items in it
//! and all its subcollections, across every library, each once. A format group with an FBX, or
//! a lone FBX, is a model or an animation by its FBX's facts (`fbx_facts`). Textures export by
//! texture set (C8): a source that is ready passes through byte for byte, and every other one is
//! converted into the export cache (C11), with its nearest `conversion.json`'s steps, then
//! OpenDAM's own (C10). A model is normalized by headless Blender (C9) into the export cache, or
//! taken from it, and lists its bounds, rig, clips and materials. An animation follows a rigged
//! model of the domain, its avatarFrom (D21): it is normalized with that model's transform, after
//! it, and lists its avatarFrom and clips.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};

use crate::db::{Database, LibraryHandle};
use crate::export::blender::{self, Follow, Job, JobKind, Runner};
use crate::export::bundle::Diff;
use crate::export::cache::CachedModel;
use crate::export::conversion::{Invalid, Walker};
use crate::export::fbx_facts::{self, FbxFacts, Reading};
use crate::export::fingerprint::{Fingerprint, Probes, Stamp};
use crate::export::infer::{self, PackImages, UnityPack};
use crate::export::names::{added_material, is_pascal, pascal};
use crate::export::packs::{self, Origin, PackSource};
use crate::export::textures::{self, Convention, Layout, LayoutEdit, Role, SourceEdits, Work};
use crate::export::{animation, cache, collision, contract, edits, hashes, settings};
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

/// How a Check treats the models headless Blender normalizes (C9, C11).
#[derive(Clone, Copy)]
pub enum Models<'a> {
    /// Runs Blender for each model the export cache doesn't hold, once the gate accepts it (C2),
    /// telling `progress` how the jobs go: the Export view's Check. With every model cached,
    /// it never looks for Blender.
    Normalize(&'a (dyn Fn(Progress) + Sync)),
    /// Takes models only from the export cache and never looks for Blender, leaving a model the
    /// cache doesn't hold out of the manifest without a problem: the Game tab's Check, and
    /// Export's re-read, which follows a Check that filled the cache.
    Cached,
}

/// How a Check's Blender jobs are going, sent once before they start and once as each ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    /// The jobs that have ended, and how many there are
    pub done: usize,
    pub total: usize,
    /// The asset whose job just ended, relative to its library root
    pub asset: Option<String>,
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
    /// What the Game tab shows of each animation, by asset id
    pub animations: HashMap<String, AnimationGame>,
}

/// What the Game tab shows of an animation (C13): the model it follows, inferred and yours, the
/// domain's rigged models it may follow, and its takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnimationGame {
    /// Its avatarFrom, by the model's asset id: inferred, the rigged model of the domain sharing
    /// the most bone names with it (D21), `None` when none shares one, and yours
    pub avatar_from: Field<Option<String>>,
    /// The domain's rigged models, which it may follow, in the Check's order
    pub models: Vec<AvatarModel>,
    /// Its takes, which the manifest lists as its clips
    pub takes: Vec<String>,
}

/// A rigged model an animation may follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvatarModel {
    pub asset_id: String,
    /// Relative to the library root
    pub file_path: String,
    pub asset_name: Option<String>,
    /// How many bone names it shares with the animation
    pub shared_bones: usize,
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
    /// By path, shared by every model of the pack
    pub images: Arc<Vec<PackImage>>,
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
            inferred: SourceEdits::inferred_role(layout),
            yours: edits.role,
        });
        TextureGame {
            layout: Field {
                inferred: LayoutEdit::of(layout).name(),
                yours: edits.layout.map(LayoutEdit::name),
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
/// inference reads through `memo`, without staging anything or running Blender.
#[cfg(test)]
pub async fn check(
    db: &Database,
    domain_id: &str,
    facts: &impl fbx_facts::Reader,
    memo: &Arc<infer::Memo>,
) -> Result<CheckReport, String> {
    inspect(db, domain_id, facts, memo, Models::Cached)
        .await
        .map(|i| i.report)
}

/// Reads the domain collection `domain_id` as a Check does, reading FBX files' facts with
/// `facts`, the files inference reads through `memo`, and its models as `models` says. An
/// unknown collection is an error; everything else that would refuse the export is a problem
/// in the report.
pub async fn inspect(
    db: &Database,
    domain_id: &str,
    facts: &impl fbx_facts::Reader,
    memo: &Arc<infer::Memo>,
    models: Models<'_>,
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
    let memo = Arc::clone(memo);
    let (evaluated, inputs) = tokio::task::spawn_blocking(move || {
        let evaluated = evaluate(&mut inputs, &memo);
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
        mut reads,
        names,
        export_packs,
        model_games,
        texture_games,
        animation_games,
        packs,
        problems: mut found,
        models: planned,
        probes,
    } = evaluated;
    for (input, probes) in inputs.iter().zip(probes) {
        let library = &input.library;
        for (path, stamp) in probes.seen {
            fingerprint.put(&library.id, format!("{path} in {}", library.name), stamp);
        }
    }
    problems.append(&mut found);

    let roots: Vec<PathBuf> = inputs.iter().map(|i| i.library.root_path.clone()).collect();
    let normalized = normalize(
        planned,
        &roots,
        models,
        settings.blender_path.as_deref(),
        &reads,
        &mut problems,
    )
    .await?;
    for (model, cached) in normalized {
        let entry = match model.follows {
            None => model_entry(&model, &reads[model.read], &export_packs, &cached),
            Some(m) => Ok(animation_entry(
                &model,
                &reads[model.read],
                reads[m].row.asset_name.as_deref().unwrap_or_default(),
                &export_packs,
                &cached,
            )),
        };
        match entry {
            Ok(entry) => {
                reads[model.read].model = Some(ReadyModel {
                    entry,
                    source: cached.fbx,
                })
            }
            Err(e) => {
                let asset = reads[model.read].row.asset.clone();
                problems.push(Problem {
                    message: format!("{}: {e}", asset.file_path),
                    assets: vec![asset],
                })
            }
        }
    }

    let domain_name = domain.clone();
    let (manifest, files, assets, problems) = tokio::task::spawn_blocking(move || {
        let (manifest, files, rows) = finish(&domain_name, reads, &export_packs, &mut problems);
        (manifest, files, rows, problems)
    })
    .await
    .map_err(|e| e.to_string())?;
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
        models: model_games,
        textures: texture_games,
        animations: animation_games,
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
    // What the Game tab shows is all inferred before Blender, so no model is normalized and no
    // manifest is built
    let evaluated = tokio::task::spawn_blocking(move || evaluate(&mut inputs, &memo))
        .await
        .map_err(|e| e.to_string())?;
    let mut assets: Vec<CheckedAsset> = evaluated.reads.into_iter().map(|r| r.row).collect();
    assets.sort_by(|a, b| a.asset.file_path.cmp(&b.asset.file_path));
    Ok(Inspection {
        report: CheckReport {
            domain: String::new(),
            bundle_path: None,
            assets,
            packs: evaluated.packs,
            problems: evaluated.problems,
            manifest: Value::Null,
            diff: None,
            staged: false,
        },
        files: Vec::new(),
        fingerprint: Fingerprint::default(),
        names: evaluated.names,
        models: evaluated.model_games,
        textures: evaluated.texture_games,
        animations: evaluated.animation_games,
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
    pack_origin(library, path.trim_matches('/'), None).await
}

/// The origin of the pack holding the file at `path` as a Check gives it (C6, A1): your override,
/// else inferred from the pack's files, both through `packs::origin`. `None` for a file in no
/// pack.
pub(crate) async fn origin_at(
    library: &LibraryHandle,
    path: &str,
) -> Result<Option<packs::Origin>, String> {
    let pack_paths: Vec<String> = sqlx::query_scalar("SELECT path FROM folders WHERE is_pack")
        .fetch_all(&library.pool)
        .await
        .map_err(|e| e.to_string())?;
    let Some(pack) = pack_at(&pack_paths, path) else {
        return Ok(None);
    };
    let overrides = structure::read_overrides(&library.pool)
        .await
        .map_err(|e| e.to_string())?;
    pack_origin(library, pack, overrides.get(pack).and_then(|o| o.origin)).await
}

/// The origin of the pack at `path` given your override `yours` (`packs::origin`), reading the
/// pack's indexed files as a check does, or `None` when no pack is at `path`.
async fn pack_origin(
    library: &LibraryHandle,
    path: &str,
    yours: Option<packs::Origin>,
) -> Result<Option<packs::Origin>, String> {
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
    tokio::task::spawn_blocking(move || Some(packs::origin(yours, &files, &mut Walker::new(&root))))
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
    /// The file to copy into the bundle: the source itself, or its export cache entry
    source: PathBuf,
}

/// A model or animation the check normalized, or took from the export cache: its manifest entry
/// and the cached FBX the bundle copies.
struct ReadyModel {
    entry: Value,
    source: PathBuf,
}

/// One asset as the check reads it, before names are compared.
struct Read {
    row: CheckedAsset,
    /// Index of its pack in the domain's packs
    pack: Option<usize>,
    /// The textures it is the first input of (D17)
    ready: Vec<ReadyTexture>,
    model: Option<ReadyModel>,
}

/// A model or animation no problem names, ready for its Blender job (C9): what the job and the
/// manifest entry need.
struct PlannedModel {
    /// Its index in the check's reads
    read: usize,
    /// For an animation, the read of the model it follows, its avatarFrom (D21), whose job runs
    /// first and whose key its own includes (C11)
    follows: Option<usize>,
    library: usize,
    /// Its exported FBX (A5), relative to the library root
    fbx: String,
    /// The FBX's sha256, from its bytes
    sha256: String,
    facts: FbxFacts,
    origin: Origin,
    /// Your quarter turns after the origin's facing (D13), 0 when you set none
    facing_quarter_turns: u8,
    /// Your rig (D20), which wins over the one Blender's result implies
    rig: Option<edits::Rig>,
    /// The FBX relative to its pack folder
    source_path: String,
    /// Its nearest `conversion.json`'s steps, which Blender's step follows (C10)
    conversion: Vec<Value>,
}

/// One texture a set exports (C8), before its file is made.
struct PlannedTexture {
    /// Its set's name
    name: String,
    output: textures::Output,
    /// Its inputs' indices in the check's reads, in the output's order
    reads: Vec<usize>,
    library: usize,
    /// Index of its pack in the domain's packs
    pack: Option<usize>,
}

/// What `evaluate` found, with the files each library's check stats, in library order.
struct Evaluated {
    reads: Vec<Read>,
    names: HashMap<String, Field<String>>,
    export_packs: Vec<packs::ExportPack>,
    /// What the Game tab shows of each model with materials, by asset id
    model_games: HashMap<String, ModelGame>,
    /// What the Game tab shows of each texture source, by asset id
    texture_games: HashMap<String, TextureGame>,
    /// What the Game tab shows of each animation, by asset id
    animation_games: HashMap<String, AnimationGame>,
    packs: Vec<DomainPack>,
    problems: Vec<Problem>,
    models: Vec<PlannedModel>,
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
    /// Its texture set, by its pack and its index in the pack's `PackImages`; `None` for an
    /// image in no pack, a set of its own
    set: Option<(PackKey, usize)>,
    game: TextureGame,
}

/// Everything a check does with files before Blender, run on a blocking thread: reads each
/// item, its pack and its file, infers the models' materials and the texture sets, compares
/// names, makes the textures and plans the models' jobs.
fn evaluate(inputs: &mut [LibraryInput], memo: &infer::Memo) -> Evaluated {
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
                origin: packs::origin(pack.edits.origin, files, walker),
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
    // each animation, as (its index in `reads`, its library and its item), and each member image
    // in a pack, as (its pack, its index in the pack's images)
    let mut models: Vec<(usize, PackKey, usize, usize)> = Vec::new();
    let mut animations: Vec<(usize, usize, usize)> = Vec::new();
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
                            } else if facts.reading() == Some(Reading::Animation) {
                                animations.push((reads.len(), l, i));
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
                                set: None,
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
                ready: Vec::new(),
                model: None,
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
    // Each pack's images as the pickers offer them, built once for all its models
    let mut pack_images: HashMap<PackKey, Arc<Vec<PackImage>>> = HashMap::new();
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
        let images = pack_images.entry(*key).or_insert_with(|| {
            let images = &catalogs[key];
            Arc::new(
                (0..images.images.len())
                    .map(|i| PackImage::of(images, i))
                    .collect(),
            )
        });
        model_games.insert(
            asset.asset_id.clone(),
            ModelGame {
                materials: materials.clone(),
                images: Arc::clone(images),
            },
        );
        row.materials = materials;
    }

    // The model each animation follows (D21): yours, else the rigged model of the domain sharing
    // the most bone names with it. A model is rigged when its rig, yours else inferred from its
    // bones, isn't none (C1: avatarFrom names a rigged model)
    let model_facts = |l: usize, i: usize| {
        let input = &inputs[l];
        let item = &input.items[i];
        (
            item,
            input.fbx[&item.id].facts.as_ref().expect("a model's facts"),
        )
    };
    let rigged: Vec<(usize, &str, &[String])> = models
        .iter()
        .filter_map(|(r, _, l, i)| {
            let (item, facts) = model_facts(*l, *i);
            let yours = edits::rig(&edits::of(&inputs[*l].edits, &item.id));
            let rig = yours.unwrap_or(edits::Rig::inferred(!facts.bones.is_empty()));
            (rig != edits::Rig::None).then_some((*r, item.id.as_str(), facts.bones.as_slice()))
        })
        .collect();
    let mut follows: HashMap<usize, usize> = HashMap::new();
    let mut animation_games: HashMap<String, AnimationGame> = HashMap::new();
    for (r, l, i) in &animations {
        let input = &inputs[*l];
        let item = &input.items[*i];
        let facts = input.fbx[&item.id]
            .facts
            .as_ref()
            .expect("an animation's facts");
        let inferred = animation::default_avatar(
            &facts.bones,
            rigged.iter().map(|(_, id, bones)| (*id, *bones)),
        );
        let yours = edits::avatar_from(&edits::of(&input.edits, &item.id));
        let asset = reads[*r].row.asset.clone();
        let mut issue = |message: String| {
            problems.push(Problem {
                message: format!("{}: {message}", asset.file_path),
                assets: vec![asset.clone()],
            })
        };
        let chosen = yours.as_deref().or(inferred);
        let model = chosen.map(|id| models.iter().find(|m| reads[m.0].row.asset.asset_id == id));
        match model {
            None => issue(
                "it has no avatarFrom: no rigged model of the domain shares a bone name with it. \
                 Add the model it animates to the domain, or choose one in its Game tab"
                    .into(),
            ),
            Some(None) => issue(
                "the model chosen as its avatarFrom isn't a model of the domain: choose another \
                 in its Game tab, or reset it"
                    .into(),
            ),
            Some(Some((m, _, ml, mi))) => {
                let path = &reads[*m].row.asset.file_path;
                let missing =
                    animation::missing_bones(&facts.bones, &model_facts(*ml, *mi).1.bones);
                if !rigged.iter().any(|(rigged, ..)| rigged == m) {
                    issue(format!(
                        "its avatarFrom {path} has no rig: give that model a rig in its Game tab, \
                         or choose another model"
                    ))
                } else if !missing.is_empty() {
                    issue(format!(
                        "its bones {} aren't bones of its avatarFrom {path}, so its clips can't \
                         play on that model",
                        missing.join(", ")
                    ))
                } else {
                    follows.insert(*r, *m);
                }
            }
        }
        let models = rigged
            .iter()
            .map(|(m, id, bones)| AvatarModel {
                asset_id: id.to_string(),
                file_path: reads[*m].row.asset.file_path.clone(),
                asset_name: reads[*m].row.asset_name.clone(),
                shared_bones: animation::shared(&facts.bones, bones),
            })
            .collect();
        animation_games.insert(
            item.id.clone(),
            AnimationGame {
                avatar_from: Field {
                    inferred: inferred.map(str::to_string),
                    yours: yours.map(Some),
                },
                models,
                takes: facts.takes.clone(),
            },
        );
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
            set: Some((key, images.set_of[image])),
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
    // Job `j` is read `first_texture + j`
    let first_texture = reads.len();
    for job in &jobs {
        texture_games.insert(job.asset.asset_id.clone(), job.game.clone());
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
        if let Err(invalid) = walkers[job.library].chain(&[&job.asset.file_path], Vec::new()) {
            invalid_conversions
                .entry((job.library, invalid.path.clone()))
                .or_insert_with(|| (invalid, Vec::new()))
                .1
                .push(job.asset.clone());
        }
        asset_names.insert(job.asset.asset_id.clone(), job.name.clone());
        reads.push(Read {
            row,
            pack: job.pack,
            ready: Vec::new(),
            model: None,
        });
    }

    // A model's FBX is covered by its nearest conversion.json too
    for (r, _, l, i) in &models {
        let input = &inputs[*l];
        let fbx = &input.fbx[&input.items[*i].id].path;
        if let Err(invalid) = walkers[*l].chain(&[fbx], Vec::new()) {
            invalid_conversions
                .entry((*l, invalid.path.clone()))
                .or_insert_with(|| (invalid, Vec::new()))
                .1
                .push(reads[*r].row.asset.clone());
        }
    }

    // Each texture set's sources go through C8's table together, by path
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut group_of: HashMap<(PackKey, usize), usize> = HashMap::new();
    for (j, job) in jobs.iter().enumerate() {
        match job.set {
            Some(set) => {
                let g = *group_of.entry(set).or_insert_with(|| {
                    groups.push(Vec::new());
                    groups.len() - 1
                });
                groups[g].push(j);
            }
            None => groups.push(vec![j]),
        }
    }
    let mut planned: Vec<PlannedTexture> = Vec::new();
    for mut group in groups {
        group.sort_by(|a, b| jobs[*a].asset.file_path.cmp(&jobs[*b].asset.file_path));
        let set_sources: Vec<textures::Source> = group
            .iter()
            .map(|j| {
                let job = &jobs[*j];
                let path = &job.asset.file_path;
                textures::Source {
                    source_path: job
                        .pack
                        .map_or_else(|| path.clone(), |p| relative_to(&sources[p].path, path)),
                    extension: job.extension.clone(),
                    layout: job.layout,
                    convention: job.convention,
                }
            })
            .collect();
        let (outputs, refused) = textures::plan(&set_sources);
        for refusal in refused {
            let assets: Vec<AssetRef> = refusal
                .sources
                .iter()
                .map(|i| jobs[group[*i]].asset.clone())
                .collect();
            problems.push(Problem {
                message: format!("{}: {}", assets[0].file_path, refusal.message),
                assets,
            });
        }
        for output in outputs {
            let first = &jobs[group[output.inputs[0]]];
            planned.push(PlannedTexture {
                name: first.name.value(),
                reads: output
                    .inputs
                    .iter()
                    .map(|i| first_texture + group[*i])
                    .collect(),
                library: first.library,
                pack: first.pack,
                output,
            });
        }
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

    names(&reads, &planned, &mut problems);

    // Each texture no problem names gets its file: its source, or the export cache's entry
    let refused: HashSet<String> = problems
        .iter()
        .flat_map(|p| &p.assets)
        .map(|a| a.asset_id.clone())
        .collect();
    for texture in &planned {
        let assets: Vec<AssetRef> = texture
            .reads
            .iter()
            .map(|r| reads[*r].row.asset.clone())
            .collect();
        let (Some(pack), true) = (texture.pack, is_pascal(&texture.name)) else {
            continue;
        };
        if assets.iter().any(|a| refused.contains(&a.asset_id)) {
            continue;
        }
        let paths: Vec<String> = assets.iter().map(|a| a.file_path.clone()).collect();
        let own = match &texture.output.work {
            Work::PassThrough { .. } => Vec::new(),
            Work::Convert { steps, .. } => steps.clone(),
        };
        let walker = &mut walkers[texture.library];
        // An invalid conversion.json is a problem already
        let Ok(conversion) = walker.chain(&[&paths[0]], own) else {
            continue;
        };
        let input = &mut inputs[texture.library];
        match texture_file(input, &mut walker.probes, &texture.output, &paths) {
            Ok((source, sha256)) => reads[texture.reads[0]].ready.push(ReadyTexture {
                name: texture.name.clone(),
                role: texture.output.role,
                extension: texture.output.extension(),
                sha256,
                source_path: relative_to(&sources[pack].path, &paths[0]),
                conversion,
                source,
            }),
            Err(e) => problems.push(Problem {
                message: format!("{}: {e}", paths[0]),
                assets,
            }),
        }
    }

    // Each model no problem names goes on to its Blender job, with its FBX's sha256, then each
    // animation, which goes with the model it follows: the models come first
    let jobs: Vec<(usize, usize, usize, Option<usize>)> = (models.iter())
        .map(|(r, _, l, i)| (*r, *l, *i, None))
        .chain(
            (animations.iter()).filter_map(|(r, l, i)| Some((*r, *l, *i, Some(*follows.get(r)?)))),
        )
        .collect();
    let mut planned_models = Vec::new();
    for (r, l, i, follows) in jobs {
        let read = &reads[r];
        let (Some(pack), Some(name)) = (read.pack, &read.row.asset_name) else {
            continue;
        };
        if !is_pascal(name) || refused.contains(&read.row.asset.asset_id) {
            continue;
        }
        if let Some(model) = follows.map(|m| &reads[m].row.asset) {
            if refused.contains(&model.asset_id) {
                problems.push(Problem {
                    message: format!(
                        "{}: the model it follows, {}, can't be exported, so neither can it",
                        read.row.asset.file_path, model.file_path
                    ),
                    assets: vec![read.row.asset.clone()],
                });
                continue;
            }
        }
        let input = &mut inputs[l];
        let item = &input.items[i];
        let fbx = &input.fbx[&item.id];
        let (fbx_path, facts) = (
            fbx.path.clone(),
            fbx.facts.clone().expect("a model's facts"),
        );
        let walker = &mut walkers[l];
        // An invalid conversion.json is a problem already
        let Ok(conversion) = walker.chain(&[&fbx_path], Vec::new()) else {
            continue;
        };
        let yours = edits::of(&input.edits, &item.id);
        match fbx_sha256(input, &mut walker.probes, &fbx_path) {
            Ok(sha256) => planned_models.push(PlannedModel {
                read: r,
                follows,
                library: l,
                source_path: relative_to(&sources[pack].path, &fbx_path),
                fbx: fbx_path,
                sha256,
                facts,
                origin: sources[pack].origin,
                facing_quarter_turns: edits::facing_quarter_turns(&yours).unwrap_or(0),
                rig: edits::rig(&yours),
                conversion,
            }),
            Err(e) => problems.push(Problem {
                message: format!(
                    "{}: its FBX {fbx_path} can't be read: {e}",
                    read.row.asset.file_path
                ),
                assets: vec![read.row.asset.clone()],
            }),
        }
    }

    let domain_packs = export_packs
        .iter()
        .cloned()
        .zip(&sources)
        .map(|(pack, source)| DomainPack {
            normal_convention: source.normal_convention(),
            library_id: source.library_id.clone(),
            path: source.path.clone(),
            pack,
        })
        .collect();
    Evaluated {
        reads,
        export_packs,
        packs: domain_packs,
        problems,
        models: planned_models,
        model_games,
        texture_games,
        animation_games,
        probes: walkers.into_iter().map(|w| w.probes).collect(),
        names: asset_names,
    }
}

/// The sha256 of the FBX at `path` in `input`'s library, from its bytes, never from the memo
/// alone, so a file rewritten at the same size and mtime can't be served its old cache entry.
/// The memo row is corrected for the next check, as for converted textures.
fn fbx_sha256(
    input: &mut LibraryInput,
    probes: &mut Probes,
    path: &str,
) -> std::io::Result<String> {
    let memoized = input.memo.sha256(probes, path)?;
    let sha256 = hashes::sha256_file(&input.library.root_path.join(path))?;
    if sha256 != memoized {
        input.memo.correct(path, sha256.clone());
    }
    Ok(sha256)
}

/// The Blender job that normalizes the model `asset` of the library at `root` (C9), from its
/// exported FBX `fbx` (relative to the root), whose bytes' sha256 is `sha256` and whose facts
/// are `facts`, in a pack of origin `origin`, turned by your `facing_quarter_turns`, and the job's
/// cache key (C11). The job writes into its cache entry. The Check and the Game tab's "Show as
/// exported" both make it here, so they share an entry.
pub(crate) fn model_job(
    asset: String,
    root: &Path,
    fbx: &str,
    sha256: &str,
    origin: Origin,
    facing_quarter_turns: u8,
    facts: &FbxFacts,
) -> (Job, String) {
    let mut job = Job {
        asset,
        kind: JobKind::Model,
        input: root.join(fbx),
        output: PathBuf::new(),
        origin_facing: origin,
        facing_quarter_turns,
        patterns: &collision::PATTERNS,
        added_material: added_material(facts.slots.iter().map(|s| s.name.as_str())),
        follow: None,
    };
    let key = cache::model_key(sha256, &job);
    job.output = cache::model_fbx(root, &key);
    (job, key)
}

/// The Blender job that normalizes the animation `asset` of the library at `root` (C9 step 4),
/// from its exported FBX `fbx` (relative to the root), whose bytes' sha256 is `sha256` and whose
/// facts are `facts`, following the model whose job is `model` and whose cache key is
/// `model_key`, and the job's cache key, which holds the model's (C11). Its origin and turns are
/// the model's. The transform it follows is read from the model's result once Blender made it,
/// so the job has no `follow` yet.
pub(crate) fn animation_job(
    asset: String,
    root: &Path,
    fbx: &str,
    sha256: &str,
    model: &Job,
    model_key: &str,
    facts: &FbxFacts,
) -> (Job, String) {
    let mut job = Job {
        asset,
        kind: JobKind::Animation,
        input: root.join(fbx),
        output: PathBuf::new(),
        origin_facing: model.origin_facing,
        facing_quarter_turns: model.facing_quarter_turns,
        patterns: &collision::PATTERNS,
        added_material: added_material(facts.slots.iter().map(|s| s.name.as_str())),
        follow: None,
    };
    let key = cache::animation_key(sha256, &job, model_key);
    job.output = cache::model_fbx(root, &key);
    (job, key)
}

/// Normalizes each planned model and animation with headless Blender (C9), or takes it from its
/// library's export cache (C11), as `models` says, and checks the source's kept slots and takes,
/// as Blender read them, against the facts the Check showed. `planned` holds the models first,
/// then the animations. An animation's job runs after its model's, since it follows the
/// transform that model's result records, and an animation is ready only with its model.
/// Blender is looked for (C2) only when an entry is missing, and a timeout, a crash, a refusal
/// or a mismatch is a problem naming the model or animation. Returns each one that is ready,
/// with its cache entry.
async fn normalize(
    planned: Vec<PlannedModel>,
    roots: &[PathBuf],
    models: Models<'_>,
    blender_path: Option<&str>,
    reads: &[Read],
    problems: &mut Vec<Problem>,
) -> Result<Vec<(PlannedModel, CachedModel)>, String> {
    let asset = |model: &PlannedModel| reads[model.read].row.asset.clone();
    let named = |model: &PlannedModel, message: String| Problem {
        message: format!("{}: {message}", asset(model).file_path),
        assets: vec![asset(model)],
    };
    let not_ready = |animation: &PlannedModel| {
        let model = animation.follows.expect("an animation's model");
        named(
            animation,
            format!(
                "the model it follows, {}, isn't ready to export, so neither is it",
                reads[model].row.asset.file_path
            ),
        )
    };
    // The model each animation follows, by its index in `planned`
    let followed: Vec<Option<usize>> = planned
        .iter()
        .map(|p| {
            let read = p.follows?;
            planned
                .iter()
                .position(|m| m.follows.is_none() && m.read == read)
        })
        .collect();
    // Each model's job, writing into its cache entry, and the entry's key, then each animation's,
    // keyed by its model's key. An animation whose model has no job has none either
    let mut jobs: Vec<Option<(Job, String)>> = planned
        .iter()
        .map(|model| {
            model.follows.is_none().then(|| {
                model_job(
                    asset(model).file_path,
                    &roots[model.library],
                    &model.fbx,
                    &model.sha256,
                    model.origin,
                    model.facing_quarter_turns,
                    &model.facts,
                )
            })
        })
        .collect();
    for (i, animation) in planned.iter().enumerate() {
        if animation.follows.is_none() {
            continue;
        }
        match followed[i] {
            Some(m) => {
                let (model_job, model_key) = jobs[m].clone().expect("a model's job");
                jobs[i] = Some(animation_job(
                    asset(animation).file_path,
                    &roots[animation.library],
                    &animation.fbx,
                    &animation.sha256,
                    &model_job,
                    &model_key,
                    &animation.facts,
                ));
            }
            None => problems.push(not_ready(animation)),
        }
    }
    let lookups: Vec<Option<(PathBuf, String)>> = planned
        .iter()
        .zip(&jobs)
        .map(|(model, job)| {
            let (_, key) = job.as_ref()?;
            Some((roots[model.library].clone(), key.clone()))
        })
        .collect();
    let mut cached: Vec<Option<CachedModel>> = tokio::task::spawn_blocking(move || {
        lookups
            .iter()
            .map(|lookup| {
                let (root, key) = lookup.as_ref()?;
                cache::cached_model(root, key)
            })
            .collect()
    })
    .await
    .map_err(|e| e.to_string())?;

    let misses: Vec<usize> = (0..planned.len())
        .filter(|i| jobs[*i].is_some() && cached[*i].is_none())
        .collect();
    if let (Models::Normalize(progress), false) = (models, misses.is_empty()) {
        match blender::gate(blender_path).await {
            Err(e) => {
                let assets: Vec<AssetRef> = misses.iter().map(|i| asset(&planned[*i])).collect();
                let paths: Vec<&str> = assets.iter().map(|a| a.file_path.as_str()).collect();
                let what = if assets.len() == 1 {
                    "the export cache doesn't hold its normalized FBX, so Blender must make it"
                } else {
                    "the export cache doesn't hold their normalized FBX files, so Blender must make them"
                };
                problems.push(Problem {
                    message: format!("{}: {what}: {e}", paths.join(", ")),
                    assets,
                });
            }
            Ok(blender) => {
                // One job per entry: models whose jobs share a key (identical FBX files with
                // the same settings in one library) share its output file, so they share the
                // job, which names them all. The models' jobs run first, then the animations'
                let shared = |animations: bool| {
                    let mut shared: Vec<Vec<usize>> = Vec::new();
                    let job = |i: usize| jobs[i].as_ref().map(|(_, key)| key);
                    for i in misses
                        .iter()
                        .filter(|i| planned[**i].follows.is_some() == animations)
                    {
                        let entry = (planned[*i].library, job(*i));
                        match shared
                            .iter_mut()
                            .find(|g| (planned[g[0]].library, job(g[0])) == entry)
                        {
                            Some(group) => group.push(*i),
                            None => shared.push(vec![*i]),
                        }
                    }
                    shared
                };
                let phases = [shared(false), shared(true)];
                let runner = Runner::new(blender);
                let total = phases.iter().map(Vec::len).sum();
                let done = std::sync::atomic::AtomicUsize::new(0);
                progress(Progress {
                    done: 0,
                    total,
                    asset: None,
                });
                for groups in &phases {
                    // An animation follows what its model's result records
                    let follow: Vec<Option<Follow>> = groups
                        .iter()
                        .map(|group| {
                            let model = followed[group[0]]?;
                            cached[model].as_ref().map(|c| Follow::of(&c.normalized))
                        })
                        .collect();
                    let runs = groups.iter().zip(follow).map(|(group, follow)| {
                        let (job, key) = jobs[group[0]].clone().expect("a missing entry's job");
                        let names: Vec<String> = group
                            .iter()
                            .map(|i| asset(&planned[*i]).file_path)
                            .collect();
                        let animation = job.kind == JobKind::Animation;
                        let job = Job {
                            asset: names.join(", "),
                            follow,
                            ..job
                        };
                        let root = roots[planned[group[0]].library].clone();
                        let (runner, done) = (&runner, &done);
                        let unfollowed = animation && job.follow.is_none();
                        let model = followed[group[0]].map(|m| asset(&planned[m]).file_path);
                        async move {
                            let result = if unfollowed {
                                Err(format!(
                                    "{}: the model it follows, {}, wasn't normalized, so it \
                                     can't follow it",
                                    job.asset,
                                    model.unwrap_or_default()
                                ))
                            } else {
                                run_job(runner, &job, key, root).await
                            };
                            let done = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                            progress(Progress {
                                done,
                                total,
                                asset: Some(job.asset.clone()),
                            });
                            (group, result)
                        }
                    });
                    for (group, result) in futures_util::future::join_all(runs).await {
                        match result {
                            Ok(entry) => {
                                for i in group {
                                    cached[*i] = Some(entry.clone());
                                }
                            }
                            Err(message) => problems.push(Problem {
                                message,
                                assets: group.iter().map(|i| asset(&planned[*i])).collect(),
                            }),
                        }
                    }
                }
            }
        }
    }

    let normalizing = matches!(models, Models::Normalize(_));
    let mut ready = Vec::new();
    // The models come first, so each animation's model is known to be ready or not
    let mut ready_reads = HashSet::new();
    for (model, cached) in planned.into_iter().zip(cached) {
        let Some(cached) = cached else {
            continue;
        };
        if model.follows.is_some_and(|m| !ready_reads.contains(&m)) {
            // Taking models only from the cache, a model it doesn't hold is left out quietly,
            // and so is an animation that follows it
            if normalizing {
                problems.push(not_ready(&model));
            }
            continue;
        }
        match blender::check_facts(&cached.normalized, &model.facts) {
            Ok(()) => {
                ready_reads.insert(model.read);
                ready.push((model, cached));
            }
            Err(e) => problems.push(named(&model, e)),
        }
    }
    Ok(ready)
}

/// Runs `job`, which writes the export cache entry of key `key` in the library at `root`, and
/// saves the entry.
async fn run_job(
    runner: &Runner,
    job: &Job,
    key: String,
    root: PathBuf,
) -> Result<CachedModel, String> {
    let normalized = runner.run(job).await?;
    tokio::task::spawn_blocking(move || cache::save_model(&root, &key, normalized))
        .await
        .map_err(|e| e.to_string())
        .and_then(|saved| saved)
        .map_err(|e| format!("{}: {e}", job.asset))
}

/// The manifest entry of `model`, read as `read`, from its normalized FBX `cached` (C9): its
/// bounds from Blender's result, `rig` yours, else none without an armature, else generic, its
/// takes as
/// `clips`, left out when there are none, and its materials as inference gives them (C7), in
/// Blender's slot order. Its `conversion` is its nearest `conversion.json`'s steps, then
/// Blender's (C10).
fn model_entry(
    model: &PlannedModel,
    read: &Read,
    export_packs: &[packs::ExportPack],
    cached: &CachedModel,
) -> Result<Value, String> {
    let normalized = &cached.normalized;
    let name = read.row.asset_name.as_deref().unwrap_or_default();
    let pack = read.pack.expect("a planned model's pack");
    let mut materials = Vec::new();
    for slot in &normalized.slots {
        let material = read
            .row
            .materials
            .iter()
            .find(|m| &m.slot == slot)
            .ok_or_else(|| {
                format!("Blender wrote the material slot {slot}, which the Check didn't infer")
            })?;
        let textures: serde_json::Map<String, Value> = material
            .textures
            .iter()
            .map(|(role, binding)| (role.as_str().to_string(), binding.asset_name.clone().into()))
            .collect();
        materials.push(json!({
            "slot": slot,
            "name": material.name,
            "surface": material.surface,
            "textures": textures,
        }));
    }
    let mut conversion = model.conversion.clone();
    conversion.push(blender::step(normalized, None));
    let mut entry = json!({
        "kind": "model",
        "assetName": name,
        "file": format!("Models/{name}.fbx"),
        "sha256": cached.sha256,
        "pack": export_packs[pack].id,
        "sourcePath": model.source_path,
        "conversion": conversion,
        "bounds": normalized.bounds,
        "rig": model.rig.unwrap_or(edits::Rig::inferred(normalized.has_armature)),
        "materials": materials,
    });
    if !normalized.takes.is_empty() {
        entry["clips"] = json!(normalized.takes);
    }
    Ok(entry)
}

/// The manifest entry of `animation`, read as `read`, from its normalized FBX `cached` (C9 step
/// 4): `avatarFrom` names the model it follows, `followed`, and `clips` are its takes. Its
/// `conversion` is its nearest `conversion.json`'s steps, then Blender's, which records the model
/// it followed (C10).
fn animation_entry(
    animation: &PlannedModel,
    read: &Read,
    followed: &str,
    export_packs: &[packs::ExportPack],
    cached: &CachedModel,
) -> Value {
    let normalized = &cached.normalized;
    let name = read.row.asset_name.as_deref().unwrap_or_default();
    let pack = read.pack.expect("a planned animation's pack");
    let mut conversion = animation.conversion.clone();
    conversion.push(blender::step(normalized, Some(followed)));
    json!({
        "kind": "animation",
        "assetName": name,
        "file": format!("Animations/{name}.fbx"),
        "sha256": cached.sha256,
        "pack": export_packs[pack].id,
        "sourcePath": animation.source_path,
        "conversion": conversion,
        "avatarFrom": followed,
        "clips": normalized.takes,
    })
}

/// The manifest of `reads`, the problems its validation adds, the manifest's files and the
/// Export view's rows, sorted.
fn finish(
    domain: &str,
    reads: Vec<Read>,
    export_packs: &[packs::ExportPack],
    problems: &mut Vec<Problem>,
) -> (Value, Vec<BundleFile>, Vec<CheckedAsset>) {
    let (manifest, mut invalid, files) = manifest(domain, &reads, export_packs, problems);
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
    (manifest, files, rows)
}

/// The file a bundle copies for `output`, made from the files `paths` (relative to the library
/// root of `input`, in the output's order), and its sha256: the source itself when it passes
/// through, else the export cache's entry (C11), converted first on a miss. A converted
/// texture's inputs are read and keyed by their bytes' own sha256s, never by the memo alone, so a
/// file rewritten at the same size and mtime can't be served its old entry; the memo row is
/// corrected for the next check.
fn texture_file(
    input: &mut LibraryInput,
    probes: &mut Probes,
    output: &textures::Output,
    paths: &[String],
) -> Result<(PathBuf, String), String> {
    let root = input.library.root_path.clone();
    let what = |i: usize| {
        if i == 0 {
            "it".to_string()
        } else {
            paths[i].clone()
        }
    };
    let mut sha256s = Vec::with_capacity(paths.len());
    for (i, path) in paths.iter().enumerate() {
        let sha256 = input
            .memo
            .sha256(probes, path)
            .map_err(|e| format!("{} can't be read: {e}", what(i)))?;
        sha256s.push(sha256);
    }
    let Work::Convert { recipe, steps } = &output.work else {
        return Ok((root.join(&paths[0]), sha256s.swap_remove(0)));
    };
    let mut bytes = Vec::with_capacity(paths.len());
    for (i, path) in paths.iter().enumerate() {
        let read = std::fs::read(root.join(path))
            .map_err(|e| format!("{} can't be read: {e}", what(i)))?;
        let sha256 = hashes::sha256_bytes(&read);
        if sha256 != sha256s[i] {
            input.memo.correct(path, sha256.clone());
            sha256s[i] = sha256;
        }
        bytes.push(read);
    }
    let key = cache::texture_key(&sha256s, steps);
    let cached = cache::texture(&root, &key, || {
        let named: Vec<(&str, &[u8])> = paths
            .iter()
            .zip(&bytes)
            .map(|(p, b)| (p.as_str(), b.as_slice()))
            .collect();
        textures::convert(*recipe, &named)
    })?;
    let sha256 = hashes::sha256_file(&cached)
        .map_err(|e| format!("its export cache entry can't be read: {e}"))?;
    Ok((cached, sha256))
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
/// materials of one name, or has no kept mesh and no take. `exported_kind` says whether it is a
/// model or an animation.
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
    if facts.reading().is_none() {
        issue(format!("{what} has no kept mesh and no take"));
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
/// name, or two textures (`textures`, a set's outputs) with one name and role. Each collision is
/// one problem naming every asset in it, a texture by all its sources. Sources collide only
/// through their outputs: a set's `_R` and `_M` maps share a name because they merge into one
/// MetallicSmoothness.
fn names(reads: &[Read], textures: &[PlannedTexture], problems: &mut Vec<Problem>) {
    // Each name and role's assets, each a model's row or a texture's sources
    type Named<'a> = BTreeMap<(bool, String, Option<Role>), Vec<Vec<&'a CheckedAsset>>>;
    let mut by_key: Named = BTreeMap::new();
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
        if matches!(row.kind, Kind::Model | Kind::Animation) {
            by_key
                .entry((false, name.clone(), None))
                .or_default()
                .push(vec![row]);
        }
    }
    for texture in textures.iter().filter(|t| is_pascal(&t.name)) {
        by_key
            .entry((true, texture.name.clone(), Some(texture.output.role)))
            .or_default()
            .push(texture.reads.iter().map(|r| &reads[*r].row).collect());
    }
    for ((texture, name, role), assets) in by_key {
        if assets.len() < 2 {
            continue;
        }
        let what = match role.filter(|_| texture) {
            Some(role) => format!("textures are named {name} with the role {}", role.as_str()),
            None => format!("models or animations are named {name}"),
        };
        let paths: Vec<&str> = assets
            .iter()
            .map(|rows| rows[0].asset.file_path.as_str())
            .collect();
        problems.push(Problem {
            message: format!(
                "{} {what}: {}. Give one another name in its Game tab",
                assets.len(),
                paths.join(" and ")
            ),
            assets: assets.iter().flatten().map(|r| r.asset.clone()).collect(),
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
        let Some(pack) = read.pack else {
            continue;
        };
        if refused.contains(read.row.asset.asset_id.as_str()) {
            continue;
        }
        if let Some(model) = &read.model {
            entries.push(Entry {
                value: model.entry.clone(),
                asset: &read.row.asset,
                pack,
                source: model.source.clone(),
            });
        }
        for texture in &read.ready {
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

    /// Inspects a new domain holding `paths`, as a Check does before staging.
    async fn inspect_domain(f: &Fixture, paths: &[&str]) -> Inspection {
        collection(f, "domain", "Props", "dg", paths).await;
        inspect(
            &f.db,
            "domain",
            &Facts::fixture(),
            &fresh_memo(),
            Models::Cached,
        )
        .await
        .unwrap()
    }

    /// The manifest entry of the texture `name` in the role `role`.
    fn entry<'a>(report: &'a CheckReport, name: &str, role: &str) -> &'a Value {
        report.manifest["assets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["assetName"] == name && a["role"] == role)
            .unwrap_or_else(|| panic!("no {name} {role} in the manifest"))
    }

    /// The bundle file `file` of `inspection`, checked against its sha256, and the file it is
    /// copied from.
    fn bundle_file<'a>(inspection: &'a Inspection, file: &str) -> (Vec<u8>, &'a Path) {
        let found = inspection
            .files
            .iter()
            .find(|b| b.file == file)
            .unwrap_or_else(|| panic!("no bundle file {file}"));
        let bytes = std::fs::read(&found.source).unwrap();
        assert_eq!(hashes::sha256_bytes(&bytes), found.sha256, "{file}");
        (bytes, &found.source)
    }

    /// The pixels of the bundle file `file`, which is a PNG.
    fn bundle_pixels(inspection: &Inspection, file: &str) -> image::DynamicImage {
        let (bytes, _) = bundle_file(inspection, file);
        image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).unwrap()
    }

    /// `pixel` repeated over a 4 by 4 fixture image.
    fn sixteen(pixel: &[u8]) -> Vec<u8> {
        pixel.repeat(16)
    }

    fn opendam_step(by: &str, from: &str, settings: Value) -> Value {
        json!({"tool": "OpenDAM", "toolVersion": "opendam-textures/1", "by": by, "from": from, "settings": settings})
    }

    #[tokio::test]
    async fn textures_that_need_work_are_converted() {
        let f = fixture().await;
        let inspection = inspect_domain(
            &f,
            &[
                "Converted/Ruins/T_Pillar_N.png",
                "Dungeon Props/Rocks/T_Slate_ORM.png",
                "Dungeon Props/Textures/T_Wall_N_DX.png",
                "Dungeon Props/Textures/T_Wall_D.jpg",
            ],
        )
        .await;
        let report = &inspection.report;

        // The step-5 guard's cases export
        assert_eq!(messages(report), Vec::<&str>::new());
        assert_valid(report);
        assert_eq!(
            texture_entries(report),
            [
                ("Pillar", "BaseColor", "Textures/Pillar_BaseColor.png"),
                ("Pillar", "Normal", "Textures/Pillar_Normal.png"),
                (
                    "Slate",
                    "MetallicSmoothness",
                    "Textures/Slate_MetallicSmoothness.png"
                ),
                ("Slate", "Occlusion", "Textures/Slate_Occlusion.png"),
                ("Wall", "BaseColor", "Textures/Wall_BaseColor.png"),
                ("Wall", "Normal", "Textures/Wall_Normal.png"),
            ]
            .map(|(n, r, p)| (n.to_string(), r.to_string(), p.to_string()))
        );

        // The Ruins pack is unreal, so its normal maps are DirectX: its steps follow the
        // conversion.json above the pack
        let ruins = report.packs.iter().find(|p| p.pack.id == "ruins").unwrap();
        assert_eq!(ruins.pack.origin, packs::Origin::Unreal);
        assert_eq!(ruins.normal_convention, Convention::DirectX);
        let normal = json!({"layout": "Normal", "role": "Normal"});
        let pillar = entry(report, "Pillar", "Normal");
        assert_eq!(pillar["sourcePath"], "T_Pillar_N.png");
        assert_eq!(
            pillar["conversion"],
            json!([
                {"tool": "UE Asset Exporter", "toolVersion": "5.4", "from": "uasset"},
                opendam_step("flip-normal-green", "png", normal.clone()),
            ])
        );
        assert_eq!(
            bundle_pixels(&inspection, "Textures/Pillar_Normal.png").as_bytes(),
            sixteen(&[128, 128, 255, 255])
        );
        assert_eq!(
            entry(report, "Wall", "Normal")["conversion"],
            json!([opendam_step("flip-normal-green", "png", normal)])
        );
        assert_eq!(
            bundle_pixels(&inspection, "Textures/Wall_Normal.png").as_bytes(),
            sixteen(&[128, 127, 255, 255])
        );

        // An ORM map is an Occlusion and a MetallicSmoothness of one name
        let orm = |role: &str| json!({"layout": "ORM", "role": role});
        assert_eq!(
            entry(report, "Slate", "Occlusion")["conversion"],
            json!([opendam_step("repack-orm", "png", orm("Occlusion"))])
        );
        assert_eq!(
            entry(report, "Slate", "MetallicSmoothness")["conversion"],
            json!([
                opendam_step("repack-orm", "png", orm("MetallicSmoothness")),
                opendam_step("invert-roughness", "png", orm("MetallicSmoothness")),
            ])
        );
        // T_Slate_ORM is (255, 128, 0)
        assert_eq!(
            bundle_pixels(&inspection, "Textures/Slate_Occlusion.png").as_bytes(),
            sixteen(&[255, 255, 255])
        );
        assert_eq!(
            bundle_pixels(&inspection, "Textures/Slate_MetallicSmoothness.png").as_bytes(),
            sixteen(&[0, 0, 0, 127])
        );

        // A JPG becomes a PNG of the same pixels, from the export cache
        let wall = entry(report, "Wall", "BaseColor");
        assert_eq!(wall["sourcePath"], "Textures/T_Wall_D.jpg");
        assert_eq!(
            wall["conversion"],
            json!([opendam_step(
                "to-png",
                "jpg",
                json!({"layout": "BaseColor", "role": "BaseColor"})
            )])
        );
        let jpg = image::open(
            f.library
                .root_path
                .join("Dungeon Props/Textures/T_Wall_D.jpg"),
        )
        .unwrap();
        let png = bundle_pixels(&inspection, "Textures/Wall_BaseColor.png");
        assert_eq!(png.color(), jpg.color());
        assert_eq!(png.as_bytes(), jpg.as_bytes());
        let (_, source) = bundle_file(&inspection, "Textures/Wall_BaseColor.png");
        assert!(source.starts_with(cache::cache_dir(&f.library.root_path)));
        // A texture that passes through is copied from its source
        let (_, source) = bundle_file(&inspection, "Textures/Pillar_BaseColor.png");
        assert_eq!(
            source,
            f.library.root_path.join("Converted/Ruins/T_Pillar_D.png")
        );
        assert_eq!(
            entry(report, "Pillar", "BaseColor")["conversion"],
            json!([{"tool": "UE Asset Exporter", "toolVersion": "5.4", "from": "uasset"}])
        );
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
        // map passes through, with its conversion.json's steps, and the stone floor's has its
        // green flipped
        assert_eq!(messages(&report), Vec::<&str>::new());
        assert_eq!(
            entry(&report, "Pillar", "Normal")["conversion"],
            json!([{"tool": "UE Asset Exporter", "toolVersion": "5.4", "from": "uasset"}])
        );
        assert_eq!(
            entry(&report, "StoneFloor", "Normal")["conversion"],
            json!([opendam_step(
                "flip-normal-green",
                "png",
                json!({"layout": "Normal", "role": "Normal"})
            )])
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
        // which is DirectX in an unreal pack, so it has its green flipped after the pack's steps
        assert_eq!(messages(&report), Vec::<&str>::new());
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
        let pillar_normal = json!([
            {"tool": "UE Asset Exporter", "toolVersion": "5.4", "from": "uasset"},
            opendam_step("flip-normal-green", "png", json!({"layout": "Normal", "role": "Normal"})),
        ]);
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
                ("Pillar", &pillar_normal),
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
                "Dungeon Props/Textures/T_Sky.exr: a .exr texture can't be exported: use png, tga, psd or tif (jpg, jpeg, bmp, webp and gif become PNG)",
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
    async fn maps_that_merge_into_one_role_are_no_name_collision_but_two_outputs_of_one_are() {
        let f = fixture().await;
        let textures = f.library.root_path.join("Dungeon Props/Textures");
        let png = std::fs::read(textures.join("T_StoneFloor_D.png")).unwrap();
        for name in [
            "Bricks_Roughness.png",
            "Bricks_Metallic.png",
            "Bricks_MaskMap.png",
            "Tiles_Roughness.png",
            "Tiles_Metallic.png",
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
                "Dungeon Props/Textures/Tiles_Roughness.png",
            ],
        )
        .await;

        // The Tiles maps share a name and merge into one MetallicSmoothness, while the Bricks
        // set's MaskMap gives a second one of its name
        assert_eq!(
            messages(&report),
            ["2 textures are named Bricks with the role MetallicSmoothness: Dungeon Props/Textures/Bricks_MaskMap.png and Dungeon Props/Textures/Bricks_Roughness.png. Give one another name in its Game tab"]
        );
        let named: Vec<&str> = report.problems[0]
            .assets
            .iter()
            .map(|a| a.file_path.as_str())
            .collect();
        assert_eq!(
            named,
            [
                "Dungeon Props/Textures/Bricks_MaskMap.png",
                "Dungeon Props/Textures/Bricks_Roughness.png",
                "Dungeon Props/Textures/Bricks_Metallic.png",
            ]
        );
        assert_eq!(
            texture_entries(&report),
            [(
                "Tiles".to_string(),
                "MetallicSmoothness".to_string(),
                "Textures/Tiles_MetallicSmoothness.png".to_string()
            )]
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
        // A take and no mesh
        let idle = "Dungeon Props/Animations/A_GuardIdle.fbx";
        let facts = Facts::fixture();
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
                "Dungeon Props/Models/SM_Barrel.obj: it has no FBX file: a model exports from its group's FBX",
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
                (idle, Kind::Animation, Status::Ready),
                (
                    "Dungeon Props/Models/SK_Guard.fbx",
                    Kind::Model,
                    Status::Ready
                ),
                (
                    "Dungeon Props/Models/SM_Barrel.obj",
                    Kind::Model,
                    Status::Problem
                ),
                (
                    "Dungeon Props/Models/SM_Chest.fbx",
                    Kind::Model,
                    Status::Ready
                ),
                (
                    "Dungeon Props/Models/SM_Gate.fbx",
                    Kind::Model,
                    Status::Ready
                ),
                (
                    "Dungeon Props/Models/SM_Pillar.fbx",
                    Kind::Model,
                    Status::Ready
                ),
            ]
        );
        let idle_row = report
            .assets
            .iter()
            .find(|a| a.kind == Kind::Animation)
            .unwrap();
        assert_eq!(idle_row.asset_name.as_deref(), Some("GuardIdle"));
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
        // The cache holds no model or animation, but the textures the materials bring in export
        let names: Vec<(String, String)> = texture_entries(&report)
            .into_iter()
            .map(|(name, role, _)| (name, role))
            .collect();
        let expected = [
            ("Gate", "BaseColor"),
            ("Gate", "MetallicSmoothness"),
            ("Guard", "BaseColor"),
            ("Guard", "MetallicSmoothness"),
            ("Guard", "Occlusion"),
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

        let expected = [
            "Dungeon Props/Models/SM_Bench.fbx: FBX has two materials named Wood",
            "Dungeon Props/Models/SM_Broken.fbx: FBX can't be read for export: can't read the FBX: not an FBX file",
            "Dungeon Props/Models/SM_Hull.fbx: FBX has no kept mesh and no take",
        ];
        assert_eq!(messages(&report), expected);
        // SM_Stool's slot finds its JPG base color by the model's name, which becomes a PNG
        assert!(texture_entries(&report).contains(&(
            "Stool".to_string(),
            "BaseColor".to_string(),
            "Textures/Stool_BaseColor.png".to_string()
        )));
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
                    Ready
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
                    Ready
                ),
                (
                    "Dungeon Props/Textures/T_Iron_R.png",
                    "Gate",
                    "Roughness",
                    dx,
                    false,
                    Ready
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
                    Ready
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

        // The textures that need converting export (AC14): the Iron maps merge into the Gate
        // MetallicSmoothness, the AORM map splits, and the JPG becomes a PNG
        let entries: Vec<(String, String)> = texture_entries(&report)
            .into_iter()
            .map(|(name, role, _)| (name, role))
            .collect();
        for (name, role) in [
            ("Gate", "MetallicSmoothness"),
            ("Guard", "MetallicSmoothness"),
            ("Guard", "Occlusion"),
            ("Stool", "BaseColor"),
        ] {
            assert!(
                entries.contains(&(name.to_string(), role.to_string())),
                "{name} {role} in {entries:?}"
            );
        }
        assert_eq!(
            entry(&report, "Gate", "MetallicSmoothness")["sourcePath"],
            "Textures/T_Iron_R.png"
        );
        assert!(messages(&report).iter().all(|m| !m.contains("/Textures/")));
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
    async fn standalone_sets_join_whole_and_export_one_texture_per_output_role() {
        let f = fixture().await;
        let inspection = inspect_domain(
            &f,
            &[
                "Dungeon Props/Rocks/T_Rock_R.png",
                "Dungeon Props/Rocks/T_Ore_M.png",
                "Dungeon Props/Rocks/T_Slate_ORM.png",
                "Dungeon Props/Textures/T_StoneFloor_N.png",
            ],
        )
        .await;
        let report = &inspection.report;

        use Status::*;
        let dx = Some(Convention::DirectX);
        assert_eq!(
            texture_rows(report),
            [
                (
                    "Dungeon Props/Rocks/T_Ore_M.png",
                    "Ore",
                    "Metallic",
                    None,
                    true,
                    Ready
                ),
                (
                    "Dungeon Props/Rocks/T_Rock_M.png",
                    "Rock",
                    "Metallic",
                    None,
                    false,
                    Ready
                ),
                // A `_DX` suffix is read before the role, in a pack whose maps are OpenGL
                (
                    "Dungeon Props/Rocks/T_Rock_N_DX.png",
                    "Rock",
                    "Normal",
                    dx,
                    false,
                    Ready
                ),
                (
                    "Dungeon Props/Rocks/T_Rock_R.png",
                    "Rock",
                    "Roughness",
                    None,
                    true,
                    Ready
                ),
                (
                    "Dungeon Props/Rocks/T_Slate_ORM.png",
                    "Slate",
                    "ORM",
                    None,
                    true,
                    Ready
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
        assert_eq!(messages(report), Vec::<&str>::new());
        assert_valid(report);
        assert_eq!(
            texture_entries(report),
            [
                ("Ore", "MetallicSmoothness"),
                ("Rock", "MetallicSmoothness"),
                ("Rock", "Normal"),
                ("Slate", "MetallicSmoothness"),
                ("Slate", "Occlusion"),
                ("StoneFloor", "BaseColor"),
                ("StoneFloor", "Normal"),
            ]
            .map(|(n, r)| (
                n.to_string(),
                r.to_string(),
                format!("Textures/{n}_{r}.png")
            ))
        );

        // T_Rock_R and T_Rock_M merge into one MetallicSmoothness, whose sourcePath is the
        // roughness map (D17): T_Rock_R is 180, so smoothness 75, and T_Rock_M is 0
        let rock = entry(report, "Rock", "MetallicSmoothness");
        assert_eq!(rock["sourcePath"], "Rocks/T_Rock_R.png");
        let merge = json!({"layout": "Roughness", "role": "MetallicSmoothness", "metallicSource": "Rocks/T_Rock_M.png"});
        assert_eq!(
            rock["conversion"],
            json!([
                opendam_step("merge-metallic-smoothness", "png", merge.clone()),
                opendam_step("invert-roughness", "png", merge),
            ])
        );
        assert_eq!(
            bundle_pixels(&inspection, "Textures/Rock_MetallicSmoothness.png").as_bytes(),
            sixteen(&[0, 0, 0, 75])
        );
        // The lone metallic map gets URP Lit's default smoothness
        let ore = entry(report, "Ore", "MetallicSmoothness");
        assert_eq!(
            ore["conversion"],
            json!([opendam_step(
                "merge-metallic-smoothness",
                "png",
                json!({"layout": "Metallic", "role": "MetallicSmoothness", "smoothness": 0.5})
            )])
        );
        assert_eq!(
            bundle_pixels(&inspection, "Textures/Ore_MetallicSmoothness.png").as_bytes(),
            sixteen(&[255, 255, 255, 128])
        );
        // T_Rock_N_DX is the Rock Normal, its green flipped from 127
        assert_eq!(
            bundle_pixels(&inspection, "Textures/Rock_Normal.png").as_bytes(),
            sixteen(&[128, 128, 255, 255])
        );
        assert_eq!(
            bundle_pixels(&inspection, "Textures/Slate_MetallicSmoothness.png").as_bytes(),
            sixteen(&[0, 0, 0, 127])
        );
    }

    #[tokio::test]
    async fn an_unchanged_domain_reuses_the_cache_and_converts_to_the_same_bytes() {
        let f = fixture().await;
        let paths = [
            "Dungeon Props/Rocks/T_Rock_R.png",
            "Dungeon Props/Rocks/T_Ore_M.png",
            "Dungeon Props/Rocks/T_Slate_ORM.png",
            "Dungeon Props/Textures/T_Wall_D.jpg",
        ];
        let first = inspect_domain(&f, &paths).await;
        let contents = |inspection: &Inspection| -> Vec<(String, Vec<u8>)> {
            let mut files: Vec<_> = inspection
                .files
                .iter()
                .map(|b| (b.file.clone(), bundle_file(inspection, &b.file).0))
                .collect();
            files.sort();
            files
        };
        let before = contents(&first);
        // The Wall member brings in its set's T_Wall_N_DX, and every file is converted
        assert_eq!(before.len(), 7);
        let cache = cache::cache_dir(&f.library.root_path).join("textures");
        let entries = || std::fs::read_dir(&cache).unwrap().count();
        assert_eq!(entries(), 7, "every converted texture is cached");

        // A second check reads the cache and lists the same bytes
        let again = inspect(
            &f.db,
            "domain",
            &Facts::fixture(),
            &fresh_memo(),
            Models::Cached,
        )
        .await
        .unwrap();
        assert_eq!(contents(&again), before);
        assert_eq!(
            again.report.manifest["assets"],
            first.report.manifest["assets"]
        );
        assert_eq!(entries(), 7);

        // A hit does no pixel work: a cache entry is used as it is
        let rock = first
            .files
            .iter()
            .find(|b| b.file == "Textures/Rock_MetallicSmoothness.png")
            .unwrap();
        let marker = textures::encode_png(&image::DynamicImage::ImageRgba8(
            image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 4])),
        ))
        .unwrap();
        std::fs::write(&rock.source, &marker).unwrap();
        let hit = inspect(
            &f.db,
            "domain",
            &Facts::fixture(),
            &fresh_memo(),
            Models::Cached,
        )
        .await
        .unwrap();
        let rock_entry = entry(&hit.report, "Rock", "MetallicSmoothness");
        assert_eq!(rock_entry["sha256"], hashes::sha256_bytes(&marker));

        // With the cache cleared, conversion makes the same bytes again
        std::fs::remove_dir_all(&cache).unwrap();
        let fresh = inspect(
            &f.db,
            "domain",
            &Facts::fixture(),
            &fresh_memo(),
            Models::Cached,
        )
        .await
        .unwrap();
        assert_eq!(contents(&fresh), before);

        // A changed source is converted again, under a new key
        let rough = f.library.root_path.join("Dungeon Props/Rocks/T_Rock_R.png");
        let mut pixels = image::open(&rough).unwrap().to_rgba8();
        pixels.put_pixel(0, 0, image::Rgba([0, 0, 0, 255]));
        pixels.save(&rough).unwrap();
        let changed = inspect(
            &f.db,
            "domain",
            &Facts::fixture(),
            &fresh_memo(),
            Models::Cached,
        )
        .await
        .unwrap();
        let pixels = bundle_pixels(&changed, "Textures/Rock_MetallicSmoothness.png");
        assert_eq!(&pixels.as_bytes()[..8], [0, 0, 0, 255, 0, 0, 0, 75]);
        assert_eq!(entries(), 8);
    }

    #[tokio::test]
    async fn a_stale_memoized_hash_is_caught_even_when_its_old_entry_is_cached() {
        let f = fixture().await;
        let path = "Dungeon Props/Textures/T_Slab_D.bmp";
        // A BMP of one size whatever its pixels, so a rewrite keeps the size its hash was taken at
        let bmp = |pixel: [u8; 3]| {
            let image = image::RgbImage::from_pixel(4, 4, image::Rgb(pixel));
            let mut bytes = Vec::new();
            image
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Bmp,
                )
                .unwrap();
            bytes
        };
        add_file(&f, path, &bmp([10, 20, 30])).await;
        let first = inspect_domain(&f, &[path]).await;
        assert_eq!(messages(&first.report), Vec::<&str>::new());
        let cache = cache::cache_dir(&f.library.root_path).join("textures");
        let entries = || std::fs::read_dir(&cache).unwrap().count();
        assert_eq!(entries(), 1);

        // The file changes, keeping its size and mtime, so the saved hash is stale, and the entry
        // made from the old bytes stays in the cache
        let source = f.library.root_path.join(path);
        let mtime = std::fs::metadata(&source).unwrap().modified().unwrap();
        let changed = bmp([200, 100, 50]);
        assert_eq!(
            changed.len(),
            std::fs::metadata(&source).unwrap().len() as usize
        );
        std::fs::write(&source, &changed).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        // The Check keys the texture by the bytes it reads, so it converts the new pixels
        let again = inspect(
            &f.db,
            "domain",
            &Facts::fixture(),
            &fresh_memo(),
            Models::Cached,
        )
        .await
        .unwrap();
        assert_eq!(messages(&again.report), Vec::<&str>::new());
        let pixels = bundle_pixels(&again, "Textures/Slab_BaseColor.png");
        assert_eq!(pixels.as_bytes(), sixteen(&[200, 100, 50]));
        assert_eq!(entries(), 2, "a new entry under the new bytes' key");
        // And the memo now holds the bytes' own hash
        let memo: String = sqlx::query_scalar("SELECT sha256 FROM file_hashes WHERE file_path = ?")
            .bind(path)
            .fetch_one(&f.library.pool)
            .await
            .unwrap();
        assert_eq!(memo, hashes::sha256_bytes(&changed));
    }

    #[tokio::test]
    async fn maps_that_cant_be_converted_together_are_one_problem_naming_them() {
        let f = fixture().await;
        let rocks = "Dungeon Props/Rocks";
        add_file(
            &f,
            &format!("{rocks}/T_Rock_Gloss.png"),
            &fixture_bytes(&format!("{rocks}/T_Rock_R.png")),
        )
        .await;
        // An 8 pixel metallic map, which can't merge with 4 pixel maps
        add_file(
            &f,
            &format!("{rocks}/T_Pebble_M.png"),
            &fixture_bytes("Dungeon Props/Textures/4K/T_Crate_N.png"),
        )
        .await;
        add_file(
            &f,
            &format!("{rocks}/T_Pebble_R.png"),
            &fixture_bytes(&format!("{rocks}/T_Rock_R.png")),
        )
        .await;
        let report = check_domain(
            &f,
            &[
                "Dungeon Props/Rocks/T_Rock_R.png",
                "Dungeon Props/Rocks/T_Pebble_R.png",
            ],
        )
        .await;

        assert_eq!(
            messages(&report),
            [
                "Dungeon Props/Rocks/T_Rock_Gloss.png: its texture set has 2 roughness or gloss maps, Rocks/T_Rock_Gloss.png and Rocks/T_Rock_R.png, and merges one into its MetallicSmoothness: remove or rename the others",
                // A file that fails to convert is found after the plan's refusals
                "Dungeon Props/Rocks/T_Pebble_R.png: its maps differ in size (4x4 and 8x8), and nothing is resized",
            ]
        );
        let paths = |i: usize| -> Vec<&str> {
            report.problems[i]
                .assets
                .iter()
                .map(|a| a.file_path.as_str())
                .collect()
        };
        assert_eq!(
            paths(0),
            [
                format!("{rocks}/T_Rock_Gloss.png"),
                format!("{rocks}/T_Rock_R.png")
            ]
        );
        assert_eq!(
            paths(1),
            [
                format!("{rocks}/T_Pebble_R.png"),
                format!("{rocks}/T_Pebble_M.png")
            ]
        );
        // The Rock set's other maps still export
        assert_eq!(
            texture_entries(&report),
            [(
                "Rock".to_string(),
                "Normal".to_string(),
                "Textures/Rock_Normal.png".to_string()
            )]
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
        let before = inspect(&f.db, "domain", &facts, &fresh_memo(), Models::Cached)
            .await
            .unwrap();
        assert_eq!(facts.read_files(), [fbx]);
        let again = inspect(&f.db, "domain", &facts, &fresh_memo(), Models::Cached)
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
        let after = inspect(&f.db, "domain", &facts, &fresh_memo(), Models::Cached)
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
        let before = inspect(&f.db, "domain", &facts, &memo, Models::Cached)
            .await
            .unwrap();
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
        let after = inspect(&f.db, "domain", &facts, &memo, Models::Cached)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn a_pick_no_longer_in_the_pack_is_a_problem_and_binds_nothing() {
        let f = fixture().await;
        let crate_ = "Dungeon Props/Models/SM_Crate.fbx";
        let crate_4k = id_of(&f.library.pool, "Dungeon Props/Textures/4K/T_Crate_N.png").await;
        let pick = json!({"Planks": {"textures": {"Normal": crate_4k}}});
        edit(&f, crate_, json!({ "materials": pick })).await;
        // The picked image leaves the pack after the write, as when its file is deleted
        sqlx::query("DELETE FROM assets WHERE id = ?")
            .bind(&crate_4k)
            .execute(&f.library.pool)
            .await
            .unwrap();
        let report = check_domain(&f, &[crate_]).await;
        assert!(
            messages(&report).contains(
                &"Dungeon Props/Models/SM_Crate.fbx: its material Planks has a Normal you picked that is no longer an image of its pack: pick another in its Game tab, or reset it"
            ),
            "{:?}",
            messages(&report)
        );
        let model = report
            .assets
            .iter()
            .find(|a| a.asset.file_path == crate_)
            .unwrap();
        assert_eq!(model.status, Status::Problem);
        assert!(
            !model.materials[0].textures.contains_key(&Role::Normal),
            "the role doesn't fall back to the inferred texture"
        );
        assert!(
            !texture_rows(&report)
                .iter()
                .any(|r| r.0.ends_with("T_Crate_N.png")),
            "no normal map joins the domain"
        );
    }

    /// A Blender at `dir/blender` that reports `version` and adds a line to `dir/ran` each time
    /// anything runs it.
    fn stub_blender(dir: &Path, version: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("blender");
        let ran = dir.join("ran");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho run >> '{}'\nprintf 'Blender {version}\\n'\n",
                ran.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn set_blender(f: &Fixture, path: &Path) {
        settings::update(&f.db.master_pool, json!({ "blenderPath": path }))
            .await
            .unwrap();
    }

    /// Inspects the domain as the Export view's Check does, running Blender for the models the
    /// cache doesn't hold, with the progress it reports.
    async fn normalize_domain(f: &Fixture, facts: &Facts) -> (Inspection, Vec<Progress>) {
        let seen = std::sync::Mutex::new(Vec::new());
        let progress = |p: Progress| seen.lock().unwrap().push(p);
        let inspection = inspect(
            &f.db,
            "domain",
            facts,
            &fresh_memo(),
            Models::Normalize(&progress),
        )
        .await
        .unwrap();
        (inspection, seen.into_inner().unwrap())
    }

    /// Puts `normalized` in the fixture library's export cache as what the FBX at `path`, with
    /// kept slots `slots`, normalizes to in a pack of origin `origin`, keyed as a Check keys it.
    fn cache_model(
        f: &Fixture,
        path: &str,
        origin: Origin,
        slots: &[&str],
        normalized: blender::Normalized,
    ) -> CachedModel {
        cache_turned_model(f, path, origin, 0, slots, normalized)
    }

    /// `cache_model`, turned by your `facing` quarter turns.
    fn cache_turned_model(
        f: &Fixture,
        path: &str,
        origin: Origin,
        facing: u8,
        slots: &[&str],
        normalized: blender::Normalized,
    ) -> CachedModel {
        let root = &f.library.root_path;
        let sha256 = hashes::sha256_file(&root.join(path)).unwrap();
        let job = Job {
            asset: path.into(),
            kind: JobKind::Model,
            input: root.join(path),
            output: PathBuf::new(),
            origin_facing: origin,
            facing_quarter_turns: facing,
            patterns: &collision::PATTERNS,
            added_material: added_material(slots.iter().copied()),
            follow: None,
        };
        let key = cache::model_key(&sha256, &job);
        let fbx = cache::model_fbx(root, &key);
        std::fs::create_dir_all(fbx.parent().unwrap()).unwrap();
        std::fs::write(&fbx, format!("{path}, normalized")).unwrap();
        cache::save_model(root, &key, normalized).unwrap()
    }

    fn model_entries(report: &CheckReport) -> Vec<&Value> {
        report.manifest["assets"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["kind"] == "model")
            .collect()
    }

    #[tokio::test]
    async fn only_a_model_the_cache_doesnt_hold_looks_for_blender() {
        let f = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        set_blender(&f, &stub_blender(dir.path(), "4.5.3 LTS")).await;
        let ran = dir.path().join("ran");
        let stool = "Dungeon Props/Models/SM_Stool.fbx";

        // Textures alone never look for Blender
        collection(
            &f,
            "domain",
            "Props",
            "dg",
            &["Dungeon Props/Rocks/T_Rock_R.png"],
        )
        .await;
        let (inspection, progress) = normalize_domain(&f, &Facts::fixture()).await;
        assert_eq!(messages(&inspection.report), Vec::<&str>::new());
        assert!(!ran.exists(), "Blender was asked its version");
        assert_eq!(progress, []);

        // A model the export cache doesn't hold needs Blender, which the gate refuses
        add_members(&f.db, &f.library, "domain", &[stool]).await;
        let (inspection, progress) = normalize_domain(&f, &Facts::fixture()).await;
        let report = &inspection.report;
        assert_eq!(
            messages(report),
            [format!(
                "{stool}: the export cache doesn't hold its normalized FBX, so Blender must make \
                 it: Blender 4.5.3 isn't 5.2.x, the only version exports accept"
            )]
        );
        assert_eq!(report.problems[0].assets.len(), 1);
        assert_eq!(report.problems[0].assets[0].file_path, stool);
        assert!(ran.exists());
        assert_eq!(progress, [], "no job ran");
        assert_eq!(model_entries(report), Vec::<&Value>::new());
        assert_valid(report);

        // Once the cache holds it, Blender isn't looked for, and the model exports
        std::fs::remove_file(&ran).unwrap();
        cache_model(
            &f,
            stool,
            Origin::Generic,
            &["Material"],
            blender::test_normalized(&["Material"], None, &[], &[]),
        );
        let (inspection, progress) = normalize_domain(&f, &Facts::fixture()).await;
        assert_eq!(messages(&inspection.report), Vec::<&str>::new());
        assert!(!ran.exists(), "Blender was asked its version");
        assert_eq!(progress, []);
        let models = model_entries(&inspection.report);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["file"], "Models/Stool.fbx");
        assert_valid(&inspection.report);
        let files: Vec<&str> = inspection.files.iter().map(|f| f.file.as_str()).collect();
        assert!(files.contains(&"Models/Stool.fbx"), "{files:?}");
    }

    #[tokio::test]
    async fn a_model_entry_has_blender_s_bounds_rig_clips_and_slot_order_after_its_conversion_json()
    {
        let f = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        set_blender(&f, &dir.path().join("no-blender")).await;
        let guard = "Dungeon Props/Models/SK_Guard.fbx";
        // Below a conversion.json with a step from uasset, so the pack is unreal
        let ruin = "Converted/Ruins/SM_Ruin.fbx";
        add_file(
            &f,
            ruin,
            &fixture_bytes("Dungeon Props/Models/SM_Pillar.fbx"),
        )
        .await;
        let facts = Facts::fixture().with(
            ruin,
            Ok(
                Facts::fixture().by_path["Dungeon Props/Models/SM_Pillar.fbx"]
                    .clone()
                    .unwrap(),
            ),
        );
        collection(&f, "domain", "Props", "dg", &[guard, ruin]).await;
        let bones = ["Hips", "Spine", "Head", "Head_end"];
        let cached_guard = cache_model(
            &f,
            guard,
            Origin::Generic,
            &["Cloth"],
            blender::test_normalized(&["Cloth"], None, &["Wave"], &bones),
        );
        // Blender's slot order, which may differ from the worker's
        let mut normalized = blender::test_normalized(&["Moss", "Stone"], None, &[], &[]);
        normalized.transform.origin_facing = Origin::Unreal;
        normalized.dropped_meshes = vec!["UCX_SM_Pillar_00".into()];
        let cached_ruin = cache_model(&f, ruin, Origin::Unreal, &["Stone", "Moss"], normalized);

        let (inspection, _) = normalize_domain(&f, &facts).await;
        let report = &inspection.report;
        assert_eq!(messages(report), Vec::<&str>::new());
        assert_valid(report);
        let models = model_entries(report);
        let blender_step = |origin: &str, dropped: usize| {
            json!({
                "tool": "Blender",
                "toolVersion": "5.2.2",
                "by": blender::NORMALIZE_VERSION,
                "from": "fbx",
                "settings": {
                    "facingQuarterTurns": 0,
                    "originFacing": origin,
                    "pivotX": 0.0,
                    "pivotY": 0.25,
                    "pivotZ": 0.0,
                    "droppedMeshes": dropped,
                },
            })
        };
        let bounds = json!({"min": [-0.5, 0.0, -0.5], "max": [0.5, 1.0, 0.5]});
        assert_eq!(
            *models[0],
            json!({
                "kind": "model",
                "assetName": "Guard",
                "file": "Models/Guard.fbx",
                "sha256": cached_guard.sha256,
                "pack": "dungeon-props",
                "sourcePath": "Models/SK_Guard.fbx",
                "conversion": [blender_step("generic", 0)],
                "bounds": bounds,
                "rig": "generic",
                "clips": ["Wave"],
                "materials": [{
                    "slot": "Cloth",
                    "name": "Cloth",
                    "surface": "opaque",
                    "textures": {
                        "BaseColor": "Guard",
                        "MetallicSmoothness": "Guard",
                        "Occlusion": "Guard",
                    },
                }],
            })
        );
        assert_eq!(
            *models[1],
            json!({
                "kind": "model",
                "assetName": "Ruin",
                "file": "Models/Ruin.fbx",
                "sha256": cached_ruin.sha256,
                "pack": "ruins",
                "sourcePath": "SM_Ruin.fbx",
                "conversion": [
                    {"tool": "UE Asset Exporter", "toolVersion": "5.4", "from": "uasset"},
                    blender_step("unreal", 1),
                ],
                "bounds": bounds,
                "rig": "none",
                "materials": [
                    {"slot": "Moss", "name": "Moss", "surface": "opaque", "textures": {}},
                    {"slot": "Stone", "name": "Stone", "surface": "opaque", "textures": {}},
                ],
            })
        );
        let file = |name: &str| inspection.files.iter().find(|f| f.file == name).unwrap();
        assert_eq!(file("Models/Guard.fbx").source, cached_guard.fbx);
        assert_eq!(file("Models/Ruin.fbx").source, cached_ruin.fbx);
    }

    #[tokio::test]
    async fn your_facing_is_in_a_models_job_and_cache_key_and_your_rig_in_its_entry() {
        let f = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        set_blender(&f, &dir.path().join("no-blender")).await;
        let stool = "Dungeon Props/Models/SM_Stool.fbx";
        collection(&f, "domain", "Props", "dg", &[stool]).await;
        let id = id_of(&f.library.pool, stool).await;
        let facts = Facts::fixture();

        // Each quarter turn is its own job, writing its own cache entry
        let root = &f.library.root_path;
        let sha256 = hashes::sha256_file(&root.join(stool)).unwrap();
        let stool_facts = facts.by_path[stool].clone().unwrap();
        let jobs: Vec<(Job, String)> = (0..4)
            .map(|turns| {
                let (job, key) = model_job(
                    stool.into(),
                    root,
                    stool,
                    &sha256,
                    Origin::Generic,
                    turns,
                    &stool_facts,
                );
                assert_eq!(job.facing_quarter_turns, turns);
                assert_eq!(job.output, cache::model_fbx(root, &key));
                (job, key)
            })
            .collect();
        let keys: HashSet<&String> = jobs.iter().map(|(_, key)| key).collect();
        assert_eq!(keys.len(), 4, "{keys:?}");

        cache_model(
            &f,
            stool,
            Origin::Generic,
            &["Material"],
            blender::test_normalized(&["Material"], None, &[], &[]),
        );
        let (inspection, _) = normalize_domain(&f, &facts).await;
        assert_eq!(messages(&inspection.report), Vec::<&str>::new());
        let models = model_entries(&inspection.report);
        assert_eq!(models[0]["rig"], "none");
        assert_eq!(
            models[0]["conversion"][0]["settings"]["facingQuarterTurns"],
            0
        );

        // Turned, the unturned entry is no hit, so Blender must make the turned one, and your
        // edits change the fingerprint Export compares
        let edit = json!({"facingQuarterTurns": 1, "rig": "generic"});
        edits::write(&f.library.pool, &id, edit.as_object().unwrap())
            .await
            .unwrap();
        let (turned, _) = normalize_domain(&f, &facts).await;
        let message = messages(&turned.report).join("\n");
        assert!(
            message.starts_with(&format!(
                "{stool}: the export cache doesn't hold its normalized FBX"
            )),
            "{message}"
        );
        assert_eq!(
            inspection.fingerprint.changes(&turned.fingerprint),
            ["your export edits in Fixture"]
        );

        let mut normalized = blender::test_normalized(&["Material"], None, &[], &[]);
        normalized.transform.facing_quarter_turns = 1;
        let cached = cache_turned_model(&f, stool, Origin::Generic, 1, &["Material"], normalized);
        let (inspection, _) = normalize_domain(&f, &facts).await;
        assert_eq!(messages(&inspection.report), Vec::<&str>::new());
        assert_valid(&inspection.report);
        let models = model_entries(&inspection.report);
        assert_eq!(models[0]["sha256"], cached.sha256);
        assert_eq!(
            models[0]["rig"], "generic",
            "yours, though it has no armature"
        );
        assert_eq!(
            models[0]["conversion"][0]["settings"]["facingQuarterTurns"],
            1
        );
    }

    #[tokio::test]
    async fn a_blender_job_that_leaves_no_result_is_a_problem_naming_its_model() {
        let f = fixture().await;
        let stool = "Dungeon Props/Models/SM_Stool.fbx";
        let totem = "Dungeon Props/Models/SM_Totem.fbx";
        collection(&f, "domain", "Props", "dg", &[stool, totem]).await;
        let paths = |problem: &Problem| -> Vec<String> {
            problem.assets.iter().map(|a| a.file_path.clone()).collect()
        };

        // Refused by the gate, both models are named in one problem
        let refused = tempfile::tempdir().unwrap();
        set_blender(&f, &stub_blender(refused.path(), "4.5.3 LTS")).await;
        let (inspection, progress) = normalize_domain(&f, &Facts::fixture()).await;
        let report = &inspection.report;
        assert_eq!(
            messages(report),
            [format!(
                "{stool}, {totem}: the export cache doesn't hold their normalized FBX files, so \
                 Blender must make them: Blender 4.5.3 isn't 5.2.x, the only version exports \
                 accept"
            )]
        );
        assert_eq!(paths(&report.problems[0]), [stool, totem]);
        assert_eq!(progress, []);

        // A Blender of the series runs each job, which exits without writing a result
        let dir = tempfile::tempdir().unwrap();
        set_blender(&f, &stub_blender(dir.path(), "5.2.2")).await;
        let (inspection, progress) = normalize_domain(&f, &Facts::fixture()).await;
        let report = &inspection.report;
        assert_eq!(
            messages(report),
            [stool, totem]
                .map(|path| format!("{path}: Blender exited without a result: Blender 5.2.2"))
        );
        assert_eq!(paths(&report.problems[0]), [stool]);
        assert_eq!(paths(&report.problems[1]), [totem]);
        for path in [stool, totem] {
            let row = report
                .assets
                .iter()
                .find(|a| a.asset.file_path == path)
                .unwrap();
            assert_eq!(row.status, Status::Problem, "{path}");
        }
        assert_eq!(model_entries(report), Vec::<&Value>::new());
        assert_valid(report);
        let entries: Vec<_> =
            std::fs::read_dir(cache::cache_dir(&f.library.root_path).join("models"))
                .map(|dir| dir.map(|e| e.unwrap().file_name()).collect())
                .unwrap_or_default();
        assert_eq!(
            entries,
            Vec::<std::ffi::OsString>::new(),
            "a failed job left a cache entry"
        );
        // Each job's end is reported, failed or not
        assert_eq!(
            progress[0],
            Progress {
                done: 0,
                total: 2,
                asset: None
            }
        );
        let dones: Vec<usize> = progress[1..].iter().map(|p| p.done).collect();
        assert_eq!(dones, [1, 2]);
        let mut ended: Vec<String> = progress[1..]
            .iter()
            .map(|p| p.asset.clone().unwrap())
            .collect();
        ended.sort();
        assert_eq!(ended, [stool, totem]);
    }

    #[tokio::test]
    async fn models_whose_jobs_share_a_cache_entry_share_one_blender_job() {
        let f = fixture().await;
        let stool = "Dungeon Props/Models/SM_Stool.fbx";
        // A byte-identical copy in the same pack: the same key, so the same output file
        let copy = "Dungeon Props/Models/SM_StoolCopy.fbx";
        add_file(&f, copy, &fixture_bytes(stool)).await;
        let facts = Facts::fixture().with(copy, Facts::fixture().by_path[stool].clone());
        collection(&f, "domain", "Props", "dg", &[stool, copy]).await;
        let dir = tempfile::tempdir().unwrap();
        set_blender(&f, &stub_blender(dir.path(), "5.2.2")).await;

        let (inspection, progress) = normalize_domain(&f, &facts).await;

        // Asked its version once, then one job, which names both models
        let runs = std::fs::read_to_string(dir.path().join("ran")).unwrap();
        assert_eq!(runs.lines().count(), 2, "{runs}");
        let report = &inspection.report;
        assert_eq!(
            messages(report),
            [format!(
                "{stool}, {copy}: Blender exited without a result: Blender 5.2.2"
            )]
        );
        let named: Vec<&str> = report.problems[0]
            .assets
            .iter()
            .map(|a| a.file_path.as_str())
            .collect();
        assert_eq!(named, [stool, copy]);
        assert_eq!(
            progress,
            [
                Progress {
                    done: 0,
                    total: 1,
                    asset: None
                },
                Progress {
                    done: 1,
                    total: 1,
                    asset: Some(format!("{stool}, {copy}"))
                },
            ]
        );
    }

    /// Export and Discard of one domain don't wait for another domain's Check while its
    /// Blender jobs run.
    #[tokio::test]
    async fn export_goes_on_while_another_domain_s_check_runs_blender() {
        use crate::export::bundle::{ExportOutcome, Exports};
        use std::os::unix::fs::PermissionsExt;
        let f = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        // Answers its version at once, and takes 6 s over each job, leaving no result
        let slow = dir.path().join("blender");
        std::fs::write(
            &slow,
            "#!/bin/sh\ncase \"$1\" in --version) printf 'Blender 5.2.2\\n';; *) sleep 6;; esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        set_blender(&f, &slow).await;
        let stool = "Dungeon Props/Models/SM_Stool.fbx";
        collection(&f, "domain", "Props", "dg", &[stool]).await;
        collection(
            &f,
            "other",
            "Rocks",
            "dg",
            &["Dungeon Props/Rocks/T_Rock_R.png"],
        )
        .await;
        let facts = Facts::fixture();
        let exports = Exports::default();
        let staged = exports
            .check(&f.db, "other", &facts, &|_| {})
            .await
            .unwrap();
        assert!(staged.staged);

        let (started, jobs_started) = tokio::sync::oneshot::channel();
        let started = std::sync::Mutex::new(Some(started));
        let progress = |p: Progress| {
            if p.done == 0 {
                let _ = started.lock().unwrap().take().unwrap().send(());
            }
        };
        let check = exports.check(&f.db, "domain", &facts, &progress);
        let export = async {
            jobs_started.await.unwrap();
            let start = std::time::Instant::now();
            let outcome = exports.export(&f.db, "other", &facts).await.unwrap();
            exports.discard("domain").await.unwrap();
            (start.elapsed(), outcome)
        };
        let (report, (waited, outcome)) = tokio::join!(check, export);

        assert!(
            matches!(outcome, ExportOutcome::Exported { .. }),
            "{outcome:?}"
        );
        assert!(
            waited < std::time::Duration::from_secs(4),
            "Export waited {waited:?} for the other domain's Blender job"
        );
        assert_eq!(
            messages(&report.unwrap()),
            [format!("{stool}: Blender exited without a result")]
        );
    }

    #[tokio::test]
    async fn an_fbx_rewritten_at_the_same_size_and_mtime_misses_its_old_cache_entry() {
        let f = fixture().await;
        let stool = "Dungeon Props/Models/SM_Stool.fbx";
        collection(&f, "domain", "Props", "dg", &[stool]).await;
        cache_model(
            &f,
            stool,
            Origin::Generic,
            &["Material"],
            blender::test_normalized(&["Material"], None, &[], &[]),
        );
        let memo_row = || async {
            sqlx::query_scalar::<_, String>("SELECT sha256 FROM file_hashes WHERE file_path = ?")
                .bind(stool)
                .fetch_one(&f.library.pool)
                .await
                .unwrap()
        };
        let source = f.library.root_path.join(stool);
        let original = std::fs::read(&source).unwrap();

        // The first Check is served the entry, and memoizes the FBX's hash
        let report = inspect_domain_cached(&f).await;
        assert_eq!(model_entries(&report).len(), 1);
        assert_eq!(memo_row().await, hashes::sha256_bytes(&original));

        // Rewritten within its mtime's resolution: same size, same mtime, other bytes
        let mtime = std::fs::metadata(&source).unwrap().modified().unwrap();
        let mut rewritten = original.clone();
        let last = rewritten.len() - 1;
        rewritten[last] ^= 0xff;
        std::fs::write(&source, &rewritten).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(
            std::fs::metadata(&source).unwrap().len(),
            original.len() as u64
        );

        let report = inspect_domain_cached(&f).await;
        assert_eq!(messages(&report), Vec::<&str>::new());
        assert_eq!(
            model_entries(&report),
            Vec::<&Value>::new(),
            "the rewritten FBX was served its old entry"
        );
        assert_eq!(memo_row().await, hashes::sha256_bytes(&rewritten));
    }

    /// Inspects the domain from the export cache alone, as the Game tab does.
    async fn inspect_domain_cached(f: &Fixture) -> CheckReport {
        inspect(
            &f.db,
            "domain",
            &Facts::fixture(),
            &fresh_memo(),
            Models::Cached,
        )
        .await
        .unwrap()
        .report
    }

    #[tokio::test]
    async fn slots_blender_and_the_worker_read_differently_are_a_problem_naming_the_model() {
        let f = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        set_blender(&f, &dir.path().join("no-blender")).await;
        let stool = "Dungeon Props/Models/SM_Stool.fbx";
        collection(&f, "domain", "Props", "dg", &[stool]).await;
        cache_model(
            &f,
            stool,
            Origin::Generic,
            &["Material"],
            blender::test_normalized(&["Wood"], None, &["Spin"], &[]),
        );

        let (inspection, _) = normalize_domain(&f, &Facts::fixture()).await;

        let report = &inspection.report;
        assert_eq!(
            messages(report),
            [format!(
                "{stool}: Blender and the preview engine read different material slots: \
                 Blender keeps Wood, and the preview engine Material"
            )]
        );
        assert_eq!(report.problems[0].assets[0].file_path, stool);
        assert_eq!(model_entries(report), Vec::<&Value>::new());
    }

    const GUARD: &str = "Dungeon Props/Models/SK_Guard.fbx";
    const IDLE: &str = "Dungeon Props/Animations/A_GuardIdle.fbx";
    const NOD: &str = "Dungeon Props/Animations/A_GuardNod.fbx";
    const WAG: &str = "Dungeon Props/Animations/A_Wag.fbx";

    async fn write_edit(f: &Fixture, path: &str, value: Value) {
        let id = id_of(&f.library.pool, path).await;
        edits::write(&f.library.pool, &id, value.as_object().unwrap())
            .await
            .unwrap();
    }

    async fn inspect_cached(f: &Fixture) -> Inspection {
        inspect(
            &f.db,
            "domain",
            &Facts::fixture(),
            &fresh_memo(),
            Models::Cached,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn an_animation_follows_the_rigged_model_sharing_the_most_bones_or_yours() {
        let f = fixture().await;
        let stool = "Dungeon Props/Models/SM_Stool.fbx";
        collection(&f, "domain", "Props", "dg", &[IDLE, NOD, WAG, stool]).await;

        // No rigged model in the domain: the stool has no bones
        let report = inspect_cached(&f).await.report;
        let none = "it has no avatarFrom: no rigged model of the domain shares a bone name with \
                    it. Add the model it animates to the domain, or choose one in its Game tab";
        assert_eq!(
            messages(&report),
            [IDLE, NOD, WAG].map(|path| format!("{path}: {none}"))
        );

        // The guard shares all of the idle's and the nod's bones, and only Hips with the wag
        add_members(&f.db, &f.library, "domain", &[GUARD]).await;
        let inspection = inspect_cached(&f).await;
        let not_guard_s = |path: &str, bones: &str, model: &str| {
            format!(
                "{path}: its bones {bones} aren't bones of its avatarFrom {model}, so its clips \
                 can't play on that model"
            )
        };
        assert_eq!(
            messages(&inspection.report),
            [not_guard_s(WAG, "Tail, Tail_end", GUARD)]
        );
        let rows = kinds_of(&inspection.report.assets.iter().collect::<Vec<_>>());
        assert!(
            rows.contains(&(IDLE, Kind::Animation, Status::Ready)),
            "{rows:?}"
        );
        assert!(
            rows.contains(&(WAG, Kind::Animation, Status::Problem)),
            "{rows:?}"
        );
        let guard = id_of(&f.library.pool, GUARD).await;
        let idle = id_of(&f.library.pool, IDLE).await;
        let game = &inspection.animations[&idle];
        assert_eq!(
            game.avatar_from,
            Field {
                inferred: Some(guard.clone()),
                yours: None
            }
        );
        assert_eq!(game.takes, ["Idle"]);
        assert_eq!(
            game.models,
            [AvatarModel {
                asset_id: guard.clone(),
                file_path: GUARD.into(),
                asset_name: Some("Guard".into()),
                shared_bones: 4,
            }]
        );
        let wag = id_of(&f.library.pool, WAG).await;
        assert_eq!(inspection.animations[&wag].models[0].shared_bones, 1);

        // Yours wins: a model with no rig is refused, and once you rig it, its bones are
        let stool_id = id_of(&f.library.pool, stool).await;
        write_edit(&f, IDLE, json!({ "avatarFrom": stool_id })).await;
        let inspection = inspect_cached(&f).await;
        assert_eq!(
            messages(&inspection.report),
            [
                format!(
                    "{IDLE}: its avatarFrom {stool} has no rig: give that model a rig in its \
                     Game tab, or choose another model"
                ),
                not_guard_s(WAG, "Tail, Tail_end", GUARD),
            ]
        );
        assert_eq!(
            inspection.animations[&idle].avatar_from.yours,
            Some(Some(stool_id.clone()))
        );
        write_edit(&f, stool, json!({"rig": "generic"})).await;
        let inspection = inspect_cached(&f).await;
        assert_eq!(
            messages(&inspection.report)[0],
            not_guard_s(IDLE, "Hips, Spine, Head, Head_end", stool)
        );
        let offered: Vec<(&str, usize)> = (inspection.animations[&idle].models.iter())
            .map(|m| (m.file_path.as_str(), m.shared_bones))
            .collect();
        assert_eq!(offered, [(GUARD, 4), (stool, 0)]);

        // A model outside the domain
        let pillar = id_of(&f.library.pool, "Dungeon Props/Models/SM_Pillar.fbx").await;
        write_edit(&f, IDLE, json!({ "avatarFrom": pillar })).await;
        assert_eq!(
            messages(&inspect_cached(&f).await.report)[0],
            format!(
                "{IDLE}: the model chosen as its avatarFrom isn't a model of the domain: choose \
                 another in its Game tab, or reset it"
            )
        );

        // Reset, it follows the guard again
        write_edit(&f, IDLE, json!({"avatarFrom": null})).await;
        assert_eq!(
            messages(&inspect_cached(&f).await.report),
            [not_guard_s(WAG, "Tail, Tail_end", GUARD)]
        );

        // A model that can't export takes the animations following it along
        write_edit(&f, stool, json!({"assetName": "Guard"})).await;
        let report = inspect_cached(&f).await.report;
        let follows_refused = |path: &str| {
            format!("{path}: the model it follows, {GUARD}, can't be exported, so neither can it")
        };
        assert_eq!(
            messages(&report),
            [
                not_guard_s(WAG, "Tail, Tail_end", GUARD),
                format!(
                    "2 models or animations are named Guard: {GUARD} and {stool}. Give one \
                     another name in its Game tab"
                ),
                // Each model's base color takes its name
                "2 textures are named Guard with the role BaseColor: Dungeon Props/Textures/\
                 T_Guard_D.png and Dungeon Props/Textures/T_Stool_D.jpg. Give one another name \
                 in its Game tab"
                    .into(),
                follows_refused(IDLE),
                follows_refused(NOD),
            ]
        );
    }

    /// What the script reports for an animation, for tests without Blender: no mesh, an
    /// armature of `bones`, the take `take`, and the transform of the model it followed.
    fn test_animation(
        take: &str,
        bones: &[&str],
        model: &blender::Normalized,
    ) -> blender::Normalized {
        let mut normalized = blender::test_normalized(&[], None, &[take], bones);
        normalized.bounds = None;
        normalized.meshes = Vec::new();
        normalized.has_mesh = false;
        normalized.transform = model.transform.clone();
        normalized
    }

    /// Puts `normalized` in the fixture library's export cache as what the animation at `path`
    /// normalizes to, following the model at `model` in a pack of origin `origin`, turned by
    /// your `facing` quarter turns, keyed as a Check keys it.
    fn cache_animation(
        f: &Fixture,
        path: &str,
        model: &str,
        origin: Origin,
        facing: u8,
        normalized: blender::Normalized,
    ) -> CachedModel {
        let root = &f.library.root_path;
        let facts = Facts::fixture();
        let fact = |path: &str| facts.by_path[path].clone().unwrap();
        let model_sha256 = hashes::sha256_file(&root.join(model)).unwrap();
        let (model_job, model_key) = model_job(
            model.into(),
            root,
            model,
            &model_sha256,
            origin,
            facing,
            &fact(model),
        );
        let sha256 = hashes::sha256_file(&root.join(path)).unwrap();
        let (job, key) = animation_job(
            path.into(),
            root,
            path,
            &sha256,
            &model_job,
            &model_key,
            &fact(path),
        );
        std::fs::create_dir_all(job.output.parent().unwrap()).unwrap();
        std::fs::write(&job.output, format!("{path}, normalized")).unwrap();
        cache::save_model(root, &key, normalized).unwrap()
    }

    #[tokio::test]
    async fn an_animation_entry_follows_its_model_whose_key_its_own_holds() {
        let f = fixture().await;
        let dir = tempfile::tempdir().unwrap();
        let no_blender = dir.path().join("no-blender");
        set_blender(&f, &no_blender).await;
        collection(&f, "domain", "Props", "dg", &[GUARD, IDLE]).await;
        let bones = ["Hips", "Spine", "Head", "Head_end"];
        let guard = blender::test_normalized(&["Cloth"], None, &["Wave"], &bones);
        cache_model(&f, GUARD, Origin::Generic, &["Cloth"], guard.clone());
        let idle = cache_animation(
            &f,
            IDLE,
            GUARD,
            Origin::Generic,
            0,
            test_animation("Idle", &bones, &guard),
        );

        let (inspection, progress) = normalize_domain(&f, &Facts::fixture()).await;
        let report = &inspection.report;
        assert_eq!(messages(report), Vec::<&str>::new());
        assert_eq!(progress, [], "a Blender job ran");
        assert_valid(report);
        let assets = report.manifest["assets"].as_array().unwrap();
        // Animations come last in the manifest
        assert_eq!(
            *assets.last().unwrap(),
            json!({
                "kind": "animation",
                "assetName": "GuardIdle",
                "file": "Animations/GuardIdle.fbx",
                "sha256": idle.sha256,
                "pack": "dungeon-props",
                "sourcePath": "Animations/A_GuardIdle.fbx",
                "conversion": [{
                    "tool": "Blender",
                    "toolVersion": "5.2.2",
                    "by": blender::NORMALIZE_VERSION,
                    "from": "fbx",
                    "settings": {
                        "facingQuarterTurns": 0,
                        "originFacing": "generic",
                        "pivotX": 0.0,
                        "pivotY": 0.25,
                        "pivotZ": 0.0,
                        "droppedMeshes": 0,
                        "followed": "Guard",
                    },
                }],
                "avatarFrom": "Guard",
                "clips": ["Idle"],
            })
        );
        let file = inspection
            .files
            .iter()
            .find(|f| f.file == "Animations/GuardIdle.fbx")
            .unwrap();
        assert_eq!(file.source, idle.fbx);

        // C11: turning the guard changes its key, and so the key of the animation following it
        write_edit(&f, GUARD, json!({"facingQuarterTurns": 1})).await;
        let (inspection, _) = normalize_domain(&f, &Facts::fixture()).await;
        assert_eq!(
            messages(&inspection.report),
            [format!(
                "{GUARD}, {IDLE}: the export cache doesn't hold their normalized FBX files, so \
                 Blender must make them: No Blender at {}",
                no_blender.display()
            )]
        );

        // An animation the cache holds, following a model it doesn't, waits for the model
        let mut turned = guard.clone();
        turned.transform.facing_quarter_turns = 1;
        cache_animation(
            &f,
            IDLE,
            GUARD,
            Origin::Generic,
            1,
            test_animation("Idle", &bones, &turned),
        );
        let (inspection, _) = normalize_domain(&f, &Facts::fixture()).await;
        assert_eq!(
            messages(&inspection.report),
            [
                format!(
                    "{GUARD}: the export cache doesn't hold its normalized FBX, so Blender must \
                     make it: No Blender at {}",
                    no_blender.display()
                ),
                format!(
                    "{IDLE}: the model it follows, {GUARD}, isn't ready to export, so neither is it"
                ),
            ]
        );
        // Taking models only from the cache, as the Game tab does, both are left out quietly
        let report = inspect_cached(&f).await.report;
        assert_eq!(messages(&report), Vec::<&str>::new());
        let kinds: Vec<&Value> = report.manifest["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| &a["kind"])
            .filter(|k| *k != "texture")
            .collect();
        assert_eq!(kinds, Vec::<&Value>::new());
    }

    /// An animation follows its model's result, so when that model's Blender job fails in the
    /// same Check, the animation is a problem naming both, and its own job never runs.
    #[tokio::test]
    async fn an_animation_whose_model_s_job_fails_is_refused_without_a_job_of_its_own() {
        let f = fixture().await;
        collection(&f, "domain", "Props", "dg", &[GUARD, IDLE]).await;
        // Answers its version, and leaves no result for any job
        let dir = tempfile::tempdir().unwrap();
        set_blender(&f, &stub_blender(dir.path(), "5.2.2")).await;

        let (inspection, progress) = normalize_domain(&f, &Facts::fixture()).await;

        let report = &inspection.report;
        assert_eq!(
            messages(report),
            [
                format!("{GUARD}: Blender exited without a result: Blender 5.2.2"),
                format!(
                    "{IDLE}: the model it follows, {GUARD}, wasn't normalized, so it can't \
                     follow it"
                ),
            ]
        );
        let named: Vec<Vec<&str>> = (report.problems.iter())
            .map(|p| p.assets.iter().map(|a| a.file_path.as_str()).collect())
            .collect();
        assert_eq!(named, [[GUARD], [IDLE]]);
        // Asked its version, then the guard's job: the animation's never ran
        let runs = std::fs::read_to_string(dir.path().join("ran")).unwrap();
        assert_eq!(runs.lines().count(), 2, "{runs}");
        // Both jobs are counted, the animation's ending without Blender
        let ended: Vec<(usize, Option<&str>)> = (progress.iter())
            .map(|p| (p.done, p.asset.as_deref()))
            .collect();
        assert_eq!(ended, [(0, None), (1, Some(GUARD)), (2, Some(IDLE))]);
        assert_eq!(progress[0].total, 2);
        let row = (report.assets.iter())
            .find(|a| a.asset.file_path == IDLE)
            .unwrap();
        assert_eq!((row.kind, row.status), (Kind::Animation, Status::Problem));
        let kinds: Vec<&Value> = (report.manifest["assets"].as_array().unwrap().iter())
            .map(|a| &a["kind"])
            .filter(|k| *k != "texture")
            .collect();
        assert_eq!(
            kinds,
            Vec::<&Value>::new(),
            "a model or animation was listed"
        );
        assert_valid(report);
    }

    /// AC6 with the Blender in `OPENDAM_BLENDER`: the fixture's animations export following their
    /// model, the one whose bones don't match is refused, the FBX whose only mesh is a `UCX_` hull
    /// exports as an animation, and turning the model re-runs the animations' jobs, whose clips
    /// then follow the turned model.
    #[tokio::test]
    async fn animations_follow_their_model_and_run_again_when_it_turns() {
        use crate::export::blender::tests::armatures;
        use crate::export::bundle::{ExportOutcome, Exports};
        let Some(program) = blender::test_blender() else {
            return;
        };
        let f = fixture().await;
        set_blender(&f, &program).await;
        collection(&f, "domain", "Props", "dg", &[GUARD, IDLE, NOD]).await;
        collection(&f, "pets", "Pets", "dg", &[GUARD, WAG]).await;
        let exports = Exports::default();
        let facts = Facts::fixture();

        // The wag's Tail isn't the guard's
        let report = exports.check(&f.db, "pets", &facts, &|_| {}).await.unwrap();
        assert_eq!(
            messages(&report),
            [format!(
                "{WAG}: its bones Tail, Tail_end aren't bones of its avatarFrom {GUARD}, so its \
                 clips can't play on that model"
            )]
        );
        assert!(!report.manifest["assets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "animation"));

        let seen = std::sync::Mutex::new(Vec::new());
        let progress = |p: Progress| seen.lock().unwrap().push(p);
        let ended = || {
            let reported = std::mem::take(&mut *seen.lock().unwrap());
            let mut ended: Vec<String> =
                (reported.iter()).filter_map(|p| p.asset.clone()).collect();
            ended.sort();
            (reported[0].total, ended)
        };
        let animations = vec![IDLE.to_string(), NOD.into()];
        let all = vec![IDLE.to_string(), NOD.into(), GUARD.into()];

        let mut turns_seen = Vec::new();
        for turns in [0, 1] {
            if turns > 0 {
                write_edit(&f, GUARD, json!({ "facingQuarterTurns": turns })).await;
            }
            let report = exports
                .check(&f.db, "domain", &facts, &progress)
                .await
                .unwrap();
            assert_eq!(messages(&report), Vec::<&str>::new());
            // The Pets check made the guard's entry, so only the animations' jobs run, and all
            // three run again once the guard turns
            let jobs = if turns == 0 { &animations } else { &all };
            assert_eq!(ended(), (jobs.len(), jobs.clone()), "{turns}");
            let ExportOutcome::Exported { bundle_path, .. } =
                exports.export(&f.db, "domain", &facts).await.unwrap()
            else {
                panic!("the export was refused");
            };
            let bundle = PathBuf::from(&bundle_path);
            assert_eq!(crate::export::bundle::verify(&bundle, &[]), Ok(()));
            let manifest: Value =
                serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).unwrap())
                    .unwrap();
            assert_eq!(contract::validate_manifest(&manifest), Ok(()));
            let model = armatures(&program, &bundle.join("Models/Guard.fbx")).await;
            for (name, clip) in [("GuardIdle", "Idle"), ("GuardNod", "Nod")] {
                let entry = (manifest["assets"].as_array().unwrap().iter())
                    .find(|a| a["assetName"] == name)
                    .unwrap();
                assert_eq!(entry["kind"], "animation");
                assert_eq!(entry["avatarFrom"], "Guard");
                assert_eq!(entry["clips"], json!([clip]));
                let settings = &entry["conversion"][0]["settings"];
                assert_eq!(settings["followed"], "Guard");
                assert_eq!(settings["facingQuarterTurns"], turns);
                let file = bundle.join(format!("Animations/{name}.fbx"));
                assert_eq!(armatures(&program, &file).await, model, "{name} {turns}");
            }
            let nod = (manifest["assets"].as_array().unwrap().iter())
                .find(|a| a["assetName"] == "GuardNod")
                .unwrap();
            assert_eq!(nod["conversion"][0]["settings"]["droppedMeshes"], 1);
            turns_seen.push(model);
        }
        assert_ne!(turns_seen[0], turns_seen[1], "the turn moved no armature");
    }

    /// Every file below `dir`, by its path relative to it.
    fn bundle_contents(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        walkdir::WalkDir::new(dir)
            .min_depth(1)
            .into_iter()
            .map(|e| e.unwrap())
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                let path = e.path().strip_prefix(dir).unwrap().display().to_string();
                (path, std::fs::read(e.path()).unwrap())
            })
            .collect()
    }

    /// The mesh names of the FBX at `fbx` as Blender imports it, and its vertices in Unity's
    /// axes.
    async fn reimport(blender: &Path, fbx: &Path) -> (Vec<String>, Vec<[f64; 3]>) {
        const READ: &str = "import bpy, json, sys
bpy.ops.wm.read_homefile(use_empty=True)
bpy.ops.import_scene.fbx(filepath=sys.argv[-1])
meshes = [o for o in bpy.data.objects if o.type == 'MESH']
points = [o.matrix_world @ v.co for o in meshes for v in o.data.vertices]
print('OPENDAM_REIMPORT', json.dumps({'meshes': sorted(o.name for o in meshes), 'vertices': [[-p.x, p.z, -p.y] for p in points]}))";
        let output = tokio::process::Command::new(blender)
            .args(["-b", "--factory-startup", "--python-exit-code", "1"])
            .args(["--python-expr", READ, "--"])
            .arg(fbx)
            .output()
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout
            .lines()
            .find_map(|l| l.strip_prefix("OPENDAM_REIMPORT "))
            .unwrap_or_else(|| panic!("{stdout}"));
        let read: Value = serde_json::from_str(line).unwrap();
        (
            serde_json::from_value(read["meshes"].clone()).unwrap(),
            serde_json::from_value(read["vertices"].clone()).unwrap(),
        )
    }

    /// AC2, AC5 and AC10, with the Blender in `OPENDAM_BLENDER`: the fixture's models, animations
    /// and textures export end to end, normalized, and export again from the export cache
    /// without Blender, byte for byte, even once a model's file is renamed.
    #[tokio::test]
    async fn the_fixture_s_models_export_normalized_and_again_from_the_cache_byte_identical() {
        use crate::export::blender::tests::{assert_size, nose};
        use crate::export::bundle::{ExportOutcome, Exports};
        let Some(program) = blender::test_blender() else {
            return;
        };
        let f = fixture().await;
        set_blender(&f, &program).await;
        let names = [
            "SK_Guard",
            "SM_Boulder",
            "SM_Crate",
            "SM_Gate",
            "SM_Lamp",
            "SM_Pillar",
            "SM_Shelf",
            "SM_Stool",
            "SM_Totem",
        ];
        let models: Vec<String> = names
            .iter()
            .map(|n| format!("Dungeon Props/Models/{n}.fbx"))
            .collect();
        let mut members: Vec<&str> = models.iter().map(String::as_str).collect();
        members.extend([IDLE, NOD, "Dungeon Props/Rocks/T_Rock_R.png"]);
        collection(&f, "domain", "Props", "dg", &members).await;
        let exports = Exports::default();
        let mut facts = Facts::fixture();
        let seen = std::sync::Mutex::new(Vec::new());
        let progress = |p: Progress| seen.lock().unwrap().push(p);

        let report = exports
            .check(&f.db, "domain", &facts, &progress)
            .await
            .unwrap();

        assert_eq!(messages(&report), Vec::<&str>::new());
        assert!(report.staged);
        // Once before the jobs, then once as each ends
        let reported = std::mem::take(&mut *seen.lock().unwrap());
        let jobs = names.len() + 2;
        assert_eq!(
            reported[0],
            Progress {
                done: 0,
                total: jobs,
                asset: None
            }
        );
        let dones: Vec<usize> = reported[1..].iter().map(|p| p.done).collect();
        assert_eq!(dones, (1..=jobs).collect::<Vec<_>>());
        let mut ended: Vec<String> = reported[1..]
            .iter()
            .map(|p| p.asset.clone().unwrap())
            .collect();
        ended.sort();
        let mut expected: Vec<String> = models.clone();
        expected.extend([IDLE.to_string(), NOD.to_string()]);
        expected.sort();
        assert_eq!(ended, expected);

        let ExportOutcome::Exported { bundle_path, .. } =
            exports.export(&f.db, "domain", &facts).await.unwrap()
        else {
            panic!("the export was refused");
        };
        let bundle = PathBuf::from(&bundle_path);
        assert_eq!(crate::export::bundle::verify(&bundle, &[]), Ok(()));
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).unwrap()).unwrap();
        let mut listed: Vec<String> = manifest["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["file"].as_str().unwrap().to_string())
            .chain(["manifest.json".to_string()])
            .collect();
        listed.sort();
        assert_eq!(
            bundle_contents(&bundle).into_keys().collect::<Vec<_>>(),
            listed
        );
        let model = |name: &str| -> Value {
            manifest["assets"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["kind"] == "model" && a["assetName"] == name)
                .unwrap_or_else(|| panic!("no model {name} in {manifest:#}"))
                .clone()
        };
        let slots = |name: &str| -> Vec<String> {
            model(name)["materials"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["slot"].as_str().unwrap().to_string())
                .collect()
        };
        let settings = |name: &str| model(name)["conversion"][0]["settings"].clone();

        // Each model, as written and as Blender reads the bundle's file back: (its name, its
        // slots, its size in metres, its kept meshes)
        type Expected<'a> = (&'a str, &'a [&'a str], [f64; 3], &'a [&'a str]);
        let expected: [Expected; 9] = [
            ("Guard", &["Cloth"], [0.5, 1.8, 0.3], &[]),
            ("Boulder", &["OpenDAMDefault"], [1.2, 0.8, 1.0], &[]),
            ("Crate", &["Planks"], [0.0; 3], &[]),
            (
                "Gate",
                &["Iron"],
                [2.0, 2.5, 0.2],
                &["SM_Gate_LOD0", "SM_Gate_LOD1"],
            ),
            (
                "Lamp",
                &["DefaultMaterial", "OpenDAMDefault"],
                [0.0; 3],
                &[],
            ),
            (
                "Pillar",
                &["Stone", "Moss"],
                [0.6, 3.0, 0.6],
                &["SM_Pillar_LOD0", "SM_Pillar_LOD1"],
            ),
            (
                "Shelf",
                &["OpenDAMDefault2", "OpenDAMDefault"],
                [0.0; 3],
                &[],
            ),
            ("Stool", &["Material"], [0.4, 0.5, 0.4], &["SM_Stool"]),
            ("Totem", &["Stone"], [0.4, 1.8, 0.4], &["SM_Totem"]),
        ];
        for (name, expected_slots, size, meshes) in expected {
            let entry = model(name);
            assert_eq!(slots(name), expected_slots, "{name}");
            let step = &entry["conversion"][0];
            assert_eq!(entry["conversion"].as_array().unwrap().len(), 1, "{name}");
            assert_eq!(step["tool"], "Blender");
            assert_eq!(step["by"], blender::NORMALIZE_VERSION);
            assert!(
                step["toolVersion"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("{}.", blender::BLENDER_SERIES)),
                "{step}"
            );
            assert_eq!(entry["file"], format!("Models/{name}.fbx"));
            let bounds: blender::Bounds = serde_json::from_value(entry["bounds"].clone()).unwrap();
            if size != [0.0; 3] {
                assert_size(bounds, size);
            }

            let (found, vertices) =
                reimport(&program, &bundle.join(format!("Models/{name}.fbx"))).await;
            if !meshes.is_empty() {
                assert_eq!(found, meshes, "{name}");
            }
            assert!(
                found.iter().all(|m| !m.starts_with("UCX_")),
                "{name} keeps a collision mesh: {found:?}"
            );
            let largest = (0..3)
                .map(|i| bounds.max[i] - bounds.min[i])
                .fold(0.0, f64::max);
            for axis in 0..3 {
                let low = vertices.iter().map(|v| v[axis]).fold(f64::MAX, f64::min);
                let high = vertices.iter().map(|v| v[axis]).fold(f64::MIN, f64::max);
                assert!(
                    (low - bounds.min[axis]).abs() <= largest * 0.001
                        && (high - bounds.max[axis]).abs() <= largest * 0.001,
                    "{name} re-imports over {low}..{high} on axis {axis}, not {bounds:?}"
                );
            }
            if name == "Totem" {
                assert_eq!(nose(&vertices), ["+Z"]);
            }
        }
        assert_eq!(model("Guard")["rig"], "generic");
        assert_eq!(model("Guard")["clips"], json!(["Wave"]));
        // The animations follow the guard, in the bundle beside it
        let animations: Vec<(&Value, &Value, &Value)> = (manifest["assets"].as_array().unwrap())
            .iter()
            .filter(|a| a["kind"] == "animation")
            .map(|a| (&a["file"], &a["avatarFrom"], &a["clips"]))
            .collect();
        assert_eq!(
            animations,
            [
                (
                    &json!("Animations/GuardIdle.fbx"),
                    &json!("Guard"),
                    &json!(["Idle"])
                ),
                (
                    &json!("Animations/GuardNod.fbx"),
                    &json!("Guard"),
                    &json!(["Nod"])
                ),
            ]
        );
        for name in ["Boulder", "Pillar", "Stool", "Totem"] {
            assert_eq!(model(name)["rig"], "none", "{name}");
            assert_eq!(model(name).get("clips"), None, "{name}");
        }
        assert_eq!(settings("Pillar")["droppedMeshes"], 1);
        assert_eq!(settings("Gate")["droppedMeshes"], 1);
        assert_eq!(settings("Boulder")["addedMaterial"], "OpenDAMDefault");
        assert_eq!(settings("Shelf")["addedMaterial"], "OpenDAMDefault2");
        assert_eq!(settings("Stool").get("addedMaterial"), None);
        // The models' textures are in the bundle beside them
        let crate_normal = &model("Crate")["materials"][0]["textures"]["Normal"];
        assert_eq!(crate_normal, "Crate");
        assert!(bundle.join("Textures/Crate_Normal.png").is_file());
        let first = bundle_contents(&bundle);

        // AC10: with no Blender to find, a second export reuses the cache, byte for byte
        let dir = tempfile::tempdir().unwrap();
        set_blender(&f, &dir.path().join("no-blender")).await;
        let report = exports
            .check(&f.db, "domain", &facts, &progress)
            .await
            .unwrap();
        assert_eq!(messages(&report), Vec::<&str>::new());
        assert_eq!(*seen.lock().unwrap(), [], "a Blender job ran");
        let diff = report.diff.as_ref().unwrap();
        assert!(diff.added.is_empty() && diff.changed.is_empty() && diff.removed.is_empty());
        let outcome = exports.export(&f.db, "domain", &facts).await.unwrap();
        assert!(
            matches!(outcome, ExportOutcome::Exported { .. }),
            "{outcome:?}"
        );
        let second = bundle_contents(&bundle);
        assert_eq!(
            first.keys().collect::<Vec<_>>(),
            second.keys().collect::<Vec<_>>()
        );
        for (path, bytes) in &first {
            if path != "manifest.json" {
                assert!(second[path] == *bytes, "{path} changed");
            }
        }
        let without_time = |bytes: &[u8]| {
            let mut manifest: Value = serde_json::from_slice(bytes).unwrap();
            manifest["exportedAt"] = Value::Null;
            manifest
        };
        assert_eq!(
            without_time(&first["manifest.json"]),
            without_time(&second["manifest.json"])
        );

        // AC5: renaming SM_Shelf changes neither its added slot's name nor its cached file
        let shelf = "Dungeon Props/Models/SM_Shelf.fbx";
        let renamed = "Dungeon Props/Models/SM_ShelfLow.fbx";
        let root = &f.library.root_path;
        std::fs::rename(root.join(shelf), root.join(renamed)).unwrap();
        crate::watcher::index_paths(&f.db, "fixture", &[root.display().to_string()])
            .await
            .unwrap();
        add_members(&f.db, &f.library, "domain", &[renamed]).await;
        let shelf_facts = facts.by_path[shelf].clone();
        facts = facts.with(renamed, shelf_facts);
        let report = exports
            .check(&f.db, "domain", &facts, &progress)
            .await
            .unwrap();
        assert_eq!(messages(&report), Vec::<&str>::new());
        assert_eq!(*seen.lock().unwrap(), [], "a Blender job ran");
        let entry = model_entries(&report)
            .into_iter()
            .find(|m| m["assetName"] == "ShelfLow")
            .unwrap()
            .clone();
        assert_eq!(entry["sha256"], model("Shelf")["sha256"]);
        assert_eq!(entry["materials"], model("Shelf")["materials"]);
        assert_eq!(entry["conversion"], model("Shelf")["conversion"]);
    }
}
