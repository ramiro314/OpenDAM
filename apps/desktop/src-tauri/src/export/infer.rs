//! Materials and texture sets (plan game-export C7, C8). A pack's images are grouped into
//! texture sets, and each slot of a model's kept meshes is given a name, a surface and a texture
//! set per role, from the first source with an answer for that role (D11):
//!
//! 1. the FBX's own texture references, resolved inside the pack;
//! 2. in a Unity pack, the slot's `.mat`, found through the model's `.fbx.meta` remap or by its
//!    name, and its textures through the pack's `.meta` GUIDs;
//! 3. images in the pack whose stem, without prefix and suffixes, is the slot's or the model's;
//! 4. images beside the model with its stem, as BaseColor.
//!
//! Nothing here is stored: inferred values are recomputed at every check (D32).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

use serde::Serialize;

use crate::export::fbx_facts::FbxFacts;
use crate::export::names::{added_material, pascal};
use crate::export::textures::{read_stem, Convention, Layout, Role, StemInfo};
use crate::export::unity;

/// An indexed image of a pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub id: String,
    /// Relative to the library root, `/`-separated
    pub path: String,
}

fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

fn stem_of(path: &str) -> &str {
    let name = file_name(path);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

fn extension_of(path: &str) -> &str {
    file_name(path).rsplit_once('.').map_or("", |(_, ext)| ext)
}

/// How a stem is compared with another (C7 step 3, C8): as its default name, lowercased, so one
/// type prefix, case and word separators don't count. `""` matches nothing.
pub fn match_key(stem: &str) -> String {
    pascal(stem).to_ascii_lowercase()
}

/// How many folders apart `a` and `b` are: steps up from `a` to the folder they share, then down
/// to `b`.
fn folder_distance(a: &str, b: &str) -> usize {
    let a: Vec<&str> = a.split('/').filter(|p| !p.is_empty()).collect();
    let b: Vec<&str> = b.split('/').filter(|p| !p.is_empty()).collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    a.len() + b.len() - 2 * common
}

/// The order folders win in when several match a role (C7): `from`'s own folder first, then the
/// nearest, then by path ignoring case.
fn folder_rank(from: &str, dir: &str) -> (usize, String) {
    (folder_distance(from, dir), dir.to_lowercase())
}

/// When a file was read: its size and mtime.
type Stamp = (u64, Option<SystemTime>);

/// Values read from files, kept for the session while each file keeps its size and mtime, as
/// `fbx_facts::Cached` keeps an FBX's facts, so a repeated Check reads none of them again.
struct FileMemo<T> {
    entries: Mutex<HashMap<PathBuf, (Stamp, T)>>,
}

impl<T: Clone> FileMemo<T> {
    fn new() -> Self {
        FileMemo {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// `read`'s value for the file at `path`, read again only when its size or mtime changed.
    /// `None` when the file can't be stat'ed, which is kept for nothing.
    fn get(&self, path: &Path, read: impl FnOnce(&Path) -> T) -> Option<T> {
        // Stamped before the read, so a file changed during it is read again next time
        let meta = std::fs::metadata(path).ok()?;
        let stamp = (meta.len(), meta.modified().ok());
        let entries = &self.entries;
        if let Some((at, value)) = entries.lock().expect("file memo").get(path) {
            if *at == stamp {
                return Some(value.clone());
            }
        }
        let value = read(path);
        entries
            .lock()
            .expect("file memo")
            .insert(path.to_path_buf(), (stamp, value.clone()));
        Some(value)
    }
}

/// Whether an image has a pixel with alpha below 0.5, by absolute path
static CUT_ALPHA: LazyLock<FileMemo<bool>> = LazyLock::new(FileMemo::new);
/// The GUID a `.meta` file gives, by the `.meta`'s absolute path
static META_GUIDS: LazyLock<FileMemo<Option<String>>> = LazyLock::new(FileMemo::new);
/// A `.mat` file as read, by absolute path
static MATERIALS: LazyLock<FileMemo<Option<unity::Material>>> = LazyLock::new(FileMemo::new);

/// Whether the image at `path` has a pixel with alpha below 0.5, decoding it without a
/// converted copy for the usual layouts. An image that can't be decoded has none.
fn read_cut_alpha(path: &Path) -> bool {
    use image::DynamicImage::*;
    match image::open(path) {
        Ok(ImageRgba8(img)) => img.pixels().any(|p| p.0[3] < 128),
        Ok(ImageLumaA8(img)) => img.pixels().any(|p| p.0[1] < 128),
        Ok(ImageRgba16(img)) => img.pixels().any(|p| p.0[3] < 32768),
        Ok(ImageLumaA16(img)) => img.pixels().any(|p| p.0[1] < 32768),
        Ok(ImageRgba32F(img)) => img.pixels().any(|p| p.0[3] < 0.5),
        Ok(img) if img.color().has_alpha() => img.to_rgba8().pixels().any(|p| p.0[3] < 128),
        _ => false,
    }
}

/// A layout someone said an image has: a Unity material's property (`strong`, which wins over
/// the image's own suffix) or an FBX reference's type (which only fills in for a missing suffix).
#[derive(Debug, Clone, Copy)]
struct Hint {
    layout: Layout,
    strong: bool,
}

/// One texture set (C8): the images of one folder whose stems match once prefix and suffixes
/// are dropped.
#[derive(Debug, Clone)]
pub struct TextureSet {
    /// Indices into `PackImages::images`, by path
    pub sources: Vec<usize>,
}

/// A pack's images in their texture sets, with what the inference has learnt about them.
#[derive(Debug)]
pub struct PackImages {
    /// By path
    pub images: Vec<Image>,
    pub stems: Vec<StemInfo>,
    /// Each image's `match_key` of its stem without prefix and suffixes
    keys: Vec<String>,
    pub sets: Vec<TextureSet>,
    /// Each image's set
    pub set_of: Vec<usize>,
    hints: HashMap<usize, Hint>,
    /// Whether an image has a pixel with alpha below 0.5, once read
    cut_alpha: HashMap<usize, bool>,
}

impl PackImages {
    /// Groups `images` into texture sets: one per folder and stem key, and one of its own for each
    /// image whose suffix names no role or layout (it is a BaseColor).
    pub fn new(mut images: Vec<Image>) -> Self {
        images.sort_by(|a, b| a.path.cmp(&b.path));
        let stems: Vec<StemInfo> = images.iter().map(|i| read_stem(stem_of(&i.path))).collect();
        let mut sets: Vec<TextureSet> = Vec::new();
        let mut by_key: HashMap<(&str, String), usize> = HashMap::new();
        let mut set_of = Vec::with_capacity(images.len());
        let keys: Vec<String> = stems.iter().map(|s| match_key(&s.base)).collect();
        for (i, (image, stem)) in images.iter().zip(&stems).enumerate() {
            let key = keys[i].clone();
            let shared = (stem.layout.is_some() && !key.is_empty())
                .then(|| by_key.get(&(dir_of(&image.path), key.clone())).copied())
                .flatten();
            let set = shared.unwrap_or_else(|| {
                sets.push(TextureSet {
                    sources: Vec::new(),
                });
                if stem.layout.is_some() && !key.is_empty() {
                    by_key.insert((dir_of(&image.path), key), sets.len() - 1);
                }
                sets.len() - 1
            });
            sets[set].sources.push(i);
            set_of.push(set);
        }
        PackImages {
            images,
            stems,
            keys,
            sets,
            set_of,
            hints: HashMap::new(),
            cut_alpha: HashMap::new(),
        }
    }

    /// The image at `path`, matched exactly.
    pub fn find(&self, path: &str) -> Option<usize> {
        self.images
            .binary_search_by(|i| i.path.as_str().cmp(path))
            .ok()
    }

    /// The image at `path`, each component matched ignoring case as `textures.py` does, the
    /// exact path first.
    fn find_ignoring_case(&self, path: &str) -> Option<usize> {
        self.find(path).or_else(|| {
            let lower = path.to_lowercase();
            self.images
                .iter()
                .position(|i| i.path.to_lowercase() == lower)
        })
    }

    /// An image's layout: a Unity material's word for it, else its suffix's, else an FBX
    /// reference's, else BaseColor (C3).
    pub fn layout(&self, image: usize) -> Layout {
        let hint = self.hints.get(&image);
        hint.filter(|h| h.strong)
            .map(|h| h.layout)
            .or(self.stems[image].layout)
            .or(hint.map(|h| h.layout))
            .unwrap_or(Layout::Role(Role::BaseColor))
    }

    /// An image's normal-map convention: its `_DX`/`_GL` suffix's, else the pack's (D14).
    pub fn convention(&self, image: usize, pack: Convention) -> Convention {
        self.stems[image].convention.unwrap_or(pack)
    }

    /// Records `hint` for `image`. The first hint of each kind stays.
    fn hint(&mut self, image: usize, hint: Hint) {
        let stays = self
            .hints
            .get(&image)
            .is_some_and(|h| h.strong || !hint.strong);
        if !stays {
            self.hints.insert(image, hint);
        }
    }

    /// The sources of set `set` that give role `role`.
    pub fn role_sources(&self, set: usize, role: Role) -> Vec<usize> {
        self.sets[set]
            .sources
            .iter()
            .copied()
            .filter(|i| self.layout(*i).outputs().contains(&role))
            .collect()
    }

    /// The stem a set's default name comes from: its first source's, without prefix and
    /// suffixes (C5).
    pub fn set_base(&self, set: usize) -> &str {
        &self.stems[self.sets[set].sources[0]].base
    }

    /// Whether `image` has a pixel with alpha below 0.5. An image that can't be decoded has
    /// none.
    fn has_cut_alpha(&mut self, root: &Path, image: usize) -> bool {
        if let Some(cut) = self.cut_alpha.get(&image) {
            return *cut;
        }
        let path = root.join(&self.images[image].path);
        let cut = CUT_ALPHA.get(&path, read_cut_alpha).unwrap_or(false);
        self.cut_alpha.insert(image, cut);
        cut
    }
}

/// What a Unity pack's files say (C7 step 2), read once per check for a pack with a model.
#[derive(Debug, Default)]
pub struct UnityPack {
    /// The pack's indexed files by the GUID their `.meta` gives them
    by_guid: HashMap<String, String>,
    /// Each `.mat` file of the pack, by path
    materials: BTreeMap<String, unity::Material>,
}

impl UnityPack {
    /// Reads the `.meta` of each of `files` (relative to `root`) and each `.mat` among them,
    /// each only when it changed since the session last read it.
    pub fn read(root: &Path, files: &[String]) -> Self {
        let mut pack = UnityPack::default();
        for file in files {
            let meta = root.join(format!("{file}.meta"));
            let guid = META_GUIDS.get(&meta, |path| {
                let text = std::fs::read_to_string(path).ok()?;
                unity::meta_guid(&text)
            });
            if let Some(guid) = guid.flatten() {
                pack.by_guid.insert(guid, file.clone());
            }
            if extension_of(file).eq_ignore_ascii_case("mat") {
                let mat = MATERIALS.get(&root.join(file), |path| {
                    let text = std::fs::read_to_string(path).ok()?;
                    Some(unity::read_material(&text))
                });
                if let Some(mat) = mat.flatten() {
                    pack.materials.insert(file.clone(), mat);
                }
            }
        }
        pack
    }

    /// The `.mat` slot `slot` of the model at `fbx` uses: the one the model's `.fbx.meta` remaps
    /// it to, else the pack's `.mat` named like it, ignoring case, nearest the model first.
    fn material(
        &self,
        remap: &BTreeMap<String, String>,
        fbx: &str,
        slot: &str,
    ) -> Option<&unity::Material> {
        let remapped = remap
            .get(slot)
            .and_then(|guid| self.by_guid.get(guid))
            .and_then(|path| self.materials.get(path));
        remapped.or_else(|| {
            self.materials
                .iter()
                .filter(|(path, _)| stem_of(path).eq_ignore_ascii_case(slot))
                .min_by_key(|(path, _)| {
                    (folder_rank(dir_of(fbx), dir_of(path)), path.to_lowercase())
                })
                .map(|(_, mat)| mat)
        })
    }
}

/// The source of an inferred texture (D11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    /// The FBX's texture reference
    Fbx,
    /// A Unity `.mat`
    Unity,
    /// A role suffix on an image named like the slot or the model
    Suffix,
    /// An image with the model's stem, as BaseColor
    Stem,
}

/// A material's texture in one role: the texture set that fills it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    /// The set's index in its pack's `PackImages`
    #[serde(skip)]
    pub set: usize,
    pub via: Via,
    /// The set's default name (C5), once the domain's sets are named
    pub asset_name: String,
    /// The set's sources that give the role, relative to the library root, never empty: a
    /// binding left with none once every model is read is dropped (`check::evaluate`)
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    Opaque,
    Cutout,
}

/// One material of a model, as inferred (C7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Material {
    pub slot: String,
    /// The slot's default name (D22)
    pub name: String,
    pub surface: Surface,
    /// Its BaseColor has alpha below 0.5 and nothing said cutout: a suggestion, which exports
    /// opaque until you accept it (D23)
    pub cutout_suggested: bool,
    /// The slot the export adds for a kept mesh with no material (C9 step 2)
    pub added: bool,
    pub textures: BTreeMap<Role, Binding>,
}

/// A model to infer the materials of, in its pack.
pub struct Model<'a> {
    /// The grid item, relative to the library root
    pub item_path: &'a str,
    /// The FBX it exports from (A5)
    pub fbx_path: &'a str,
    pub facts: &'a FbxFacts,
}

/// FBX texture reference types, in the order their answers are taken, with the layout each
/// gives an image whose suffix names none.
const REFERENCE_TYPES: [(&str, Layout); 7] = [
    ("baseColor", Layout::Role(Role::BaseColor)),
    ("normal", Layout::Role(Role::Normal)),
    ("metalness", Layout::Metallic),
    ("roughness", Layout::Roughness),
    ("occlusion", Layout::Role(Role::Occlusion)),
    ("emissive", Layout::Role(Role::Emission)),
    ("height", Layout::Role(Role::Height)),
];

/// Unity texture properties in the order their answers are taken (C7 step 2).
const UNITY_PROPERTIES: [&str; 12] = [
    "_BaseMap",
    "_MainTex",
    "_BaseColorMap",
    "_BumpMap",
    "_NormalMap",
    "_MetallicGlossMap",
    "_MaskMap",
    "_OcclusionMap",
    "_EmissionMap",
    "_EmissiveColorMap",
    "_ParallaxMap",
    "_HeightMap",
];

/// `reference` (a path, `\` read as `/`) joined to `dir` lexically, or `None` when it
/// climbs above the library root.
fn join_lexically(dir: &str, reference: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in reference.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

/// The image an FBX texture reference names (C7 step 1), as `textures.py` resolves it: the
/// given path, then the relative path from the FBX's folder, then by file name within the pack,
/// nearest the FBX first. Only the pack's indexed images are found, so nothing outside the
/// library and the pack is.
fn resolve(
    images: &PackImages,
    root: &Path,
    fbx: &str,
    reference: &crate::export::fbx_facts::TextureRef,
) -> Option<usize> {
    if let Some(given) = &reference.file {
        let given = given.replace('\\', "/");
        let roots = [Some(root.to_path_buf()), std::fs::canonicalize(root).ok()];
        let found = roots.iter().flatten().find_map(|r| {
            let relative = Path::new(&given).strip_prefix(r).ok()?.to_str()?;
            images.find(&join_lexically("", relative)?)
        });
        if found.is_some() {
            return found;
        }
    }
    if let Some(relative) = &reference.relative {
        let found =
            join_lexically(dir_of(fbx), relative).and_then(|p| images.find_ignoring_case(&p));
        if found.is_some() {
            return found;
        }
    }
    let name = [&reference.relative, &reference.file]
        .into_iter()
        .flatten()
        .map(|r| r.rsplit(['/', '\\']).next().unwrap_or(r))
        .find(|n| !n.is_empty())?;
    let (stem, extension) = (stem_of(name), extension_of(name));
    (0..images.images.len())
        .filter(|i| stem_of(&images.images[*i].path).eq_ignore_ascii_case(stem))
        .min_by_key(|i| {
            let path = &images.images[*i].path;
            (
                folder_rank(dir_of(fbx), dir_of(path)),
                !extension_of(path).eq_ignore_ascii_case(extension),
                path.to_lowercase(),
            )
        })
}

/// What one slot has learnt so far: the set answering each role, and whether a Unity material
/// said cutout.
#[derive(Default)]
struct Answers {
    textures: BTreeMap<Role, (usize, Via)>,
    cutout: bool,
}

impl Answers {
    /// Takes `set` for the roles `layout` gives that no earlier source answered.
    fn answer(&mut self, layout: Layout, set: usize, via: Via) {
        for role in layout.outputs() {
            self.textures.entry(*role).or_insert((set, via));
        }
    }
}

/// The materials of `model`, whose pack's images are `images` in the library at `root`. In a
/// Unity pack, `unity` holds its `.meta` and `.mat` files. The slots are the kept meshes' (C7),
/// each once, then the one the export adds for a kept mesh with no material.
pub fn materials(
    images: &mut PackImages,
    root: &Path,
    unity: Option<&UnityPack>,
    model: &Model,
) -> Vec<Material> {
    let mut slots: Vec<&str> = Vec::new();
    for slot in &model.facts.slots {
        if !slots.contains(&slot.name.as_str()) {
            slots.push(&slot.name);
        }
    }
    let added = model
        .facts
        .kept_mesh_without_material
        .then(|| added_material(slots.iter().copied()));
    let remap = unity
        .and_then(|_| std::fs::read_to_string(root.join(format!("{}.meta", model.fbx_path))).ok())
        .map(|meta| unity::material_remap(&meta))
        .unwrap_or_default();

    let names = slots
        .iter()
        .map(|s| (s.to_string(), false))
        .chain(added.map(|a| (a, true)));
    let mut materials = Vec::new();
    for (slot, is_added) in names {
        let mut answers = Answers::default();

        // 1. The FBX's references
        let references = model.facts.slots.iter().find(|s| s.name == slot);
        for (kind, layout) in REFERENCE_TYPES {
            let refs = references.and_then(|s| s.textures.get(kind));
            for reference in refs.into_iter().flatten() {
                if let Some(image) = resolve(images, root, model.fbx_path, reference) {
                    images.hint(
                        image,
                        Hint {
                            layout,
                            strong: false,
                        },
                    );
                    // The layout the image now has, which its set's sources are read by: an
                    // image referenced as two types gives the roles of the first only
                    answers.answer(images.layout(image), images.set_of[image], Via::Fbx);
                    break;
                }
            }
        }

        // 2. A Unity material
        let mat = unity.and_then(|u| Some((u, u.material(&remap, model.fbx_path, &slot)?)));
        if let Some((unity, mat)) = mat {
            answers.cutout = mat.alpha_clip;
            for property in UNITY_PROPERTIES {
                let image = mat
                    .textures
                    .get(property)
                    .and_then(|guid| unity.by_guid.get(guid))
                    .and_then(|path| images.find(path));
                let (Some(image), Some(layout)) = (image, unity::property_layout(property)) else {
                    continue;
                };
                images.hint(
                    image,
                    Hint {
                        layout,
                        strong: true,
                    },
                );
                answers.answer(images.layout(image), images.set_of[image], Via::Unity);
            }
        }

        // 3. Suffixes on images named like the slot, then like the model
        let model_stem = stem_of(model.item_path);
        let mut keys = vec![match_key(&slot), match_key(model_stem)];
        keys.dedup();
        for key in keys.into_iter().filter(|k| !k.is_empty()) {
            // Each role from one set, chosen by its folder
            let mut best: BTreeMap<Role, ((usize, String), usize)> = BTreeMap::new();
            for (i, stem) in images.stems.iter().enumerate() {
                let Some(layout) = stem.layout else { continue };
                if images.keys[i] != key {
                    continue;
                }
                let rank = folder_rank(dir_of(model.fbx_path), dir_of(&images.images[i].path));
                for role in layout.outputs() {
                    let entry = best
                        .entry(*role)
                        .or_insert((rank.clone(), images.set_of[i]));
                    if rank < entry.0 {
                        *entry = (rank.clone(), images.set_of[i]);
                    }
                }
            }
            for (role, (_, set)) in best {
                answers.textures.entry(role).or_insert((set, Via::Suffix));
            }
        }

        // 4. An image beside the model with its stem
        if !answers.textures.contains_key(&Role::BaseColor) {
            let beside = [model.item_path, model.fbx_path];
            let same_stem = (0..images.images.len())
                .filter(|i| {
                    let path = &images.images[*i].path;
                    beside.iter().any(|m| {
                        dir_of(m) == dir_of(path) && stem_of(m).eq_ignore_ascii_case(stem_of(path))
                    })
                })
                .min_by_key(|i| images.images[*i].path.to_lowercase());
            if let Some(image) = same_stem {
                let layout = Layout::Role(Role::BaseColor);
                images.hint(
                    image,
                    Hint {
                        layout,
                        strong: false,
                    },
                );
                // Only as a BaseColor: an image another source called something else isn't
                if images.layout(image).outputs().contains(&Role::BaseColor) {
                    answers.answer(layout, images.set_of[image], Via::Stem);
                }
            }
        }

        let cutout_suggested = !answers.cutout
            && answers
                .textures
                .get(&Role::BaseColor)
                .is_some_and(|(set, _)| {
                    images
                        .role_sources(*set, Role::BaseColor)
                        .into_iter()
                        .any(|i| images.has_cut_alpha(root, i))
                });
        materials.push(Material {
            name: pascal(&slot),
            surface: if answers.cutout {
                Surface::Cutout
            } else {
                Surface::Opaque
            },
            cutout_suggested,
            added: is_added,
            textures: answers
                .textures
                .into_iter()
                .map(|(role, (set, via))| {
                    (
                        role,
                        Binding {
                            set,
                            via,
                            asset_name: String::new(),
                            files: Vec::new(),
                        },
                    )
                })
                .collect(),
            slot,
        });
    }
    materials
}

/// One model's use of a texture set: the model's index, the name of the first material using
/// the set, and the roles the model uses it in.
pub type SetUse = (usize, String, Vec<Role>);

/// A texture set's default name (D24, C5): the model's `assetName` when one model uses it, or
/// `<AssetName><MaterialName>` when that model has more than one set in a role this set fills
/// for it, else its stem without prefix and suffixes.
pub fn set_name(
    images: &PackImages,
    set: usize,
    users: &[SetUse],
    model_names: &[String],
    model_sets: impl Fn(usize, Role) -> usize,
) -> String {
    let mut models: Vec<usize> = users.iter().map(|(m, _, _)| *m).collect();
    models.sort_unstable();
    models.dedup();
    match models.as_slice() {
        [model] => {
            let crowded = users
                .iter()
                .flat_map(|(_, _, roles)| roles)
                .any(|role| model_sets(*model, *role) > 1);
            let name = &model_names[*model];
            if crowded {
                format!("{name}{}", users[0].1)
            } else {
                name.clone()
            }
        }
        _ => pascal(images.set_base(set)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::fbx_facts::{Slot, TextureRef};

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/game-export/library"
    );

    fn root() -> &'static Path {
        Path::new(FIXTURE)
    }

    /// The indexed images of the fixture pack at `pack`, as the watcher sees them.
    fn pack_images(pack: &str) -> PackImages {
        let images = walkdir::WalkDir::new(root().join(pack))
            .into_iter()
            .map(Result::unwrap)
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                e.path()
                    .strip_prefix(root())
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .filter(|p| {
                let ext = extension_of(p).to_ascii_lowercase();
                ["png", "jpg", "tga", "exr", "psd"].contains(&ext.as_str())
            })
            .map(|path| Image {
                id: path.clone(),
                path,
            })
            .collect();
        PackImages::new(images)
    }

    fn pack_files(pack: &str) -> Vec<String> {
        walkdir::WalkDir::new(root().join(pack))
            .into_iter()
            .map(Result::unwrap)
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                e.path()
                    .strip_prefix(root())
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .filter(|p| !p.ends_with(".meta"))
            .collect()
    }

    fn fixture_facts(path: &str) -> FbxFacts {
        let all: HashMap<String, FbxFacts> = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/game-export/fbx_facts.json"
        )))
        .unwrap();
        all[path].clone()
    }

    /// Each material of the fixture model at `path` as (slot, name, surface, suggested, added,
    /// role -> (set's first file, via)).
    fn infer(pack: &str, path: &str) -> (PackImages, Vec<Material>) {
        let mut images = pack_images(pack);
        let facts = fixture_facts(path);
        let unity = path
            .starts_with("Unity Moss")
            .then(|| UnityPack::read(root(), &pack_files(pack)));
        let model = Model {
            item_path: path,
            fbx_path: path,
            facts: &facts,
        };
        let materials = materials(&mut images, root(), unity.as_ref(), &model);
        (images, materials)
    }

    /// `material`'s textures as role -> (the set's sources giving it, via).
    fn textures(images: &PackImages, material: &Material) -> Vec<(Role, Vec<String>, Via)> {
        material
            .textures
            .iter()
            .map(|(role, b)| {
                let files = images
                    .role_sources(b.set, *role)
                    .into_iter()
                    .map(|i| images.images[i].path.clone())
                    .collect();
                (*role, files, b.via)
            })
            .collect()
    }

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn images_group_into_sets_by_folder_and_stem() {
        let images = pack_images("Dungeon Props");
        let set_files = |path: &str| -> Vec<&str> {
            let set = images.set_of[images.find(path).unwrap()];
            images.sets[set]
                .sources
                .iter()
                .map(|i| images.images[*i].path.as_str())
                .collect()
        };
        assert_eq!(
            set_files("Dungeon Props/Rocks/T_Rock_M.png"),
            [
                "Dungeon Props/Rocks/T_Rock_M.png",
                "Dungeon Props/Rocks/T_Rock_N_DX.png",
                "Dungeon Props/Rocks/T_Rock_R.png",
            ]
        );
        assert_eq!(
            set_files("Dungeon Props/Textures/T_StoneFloor_N.png"),
            [
                "Dungeon Props/Textures/T_StoneFloor_D.png",
                "Dungeon Props/Textures/T_StoneFloor_N.png",
            ]
        );
        // One set per folder: the 1K and 4K normal maps aren't one
        assert_eq!(
            set_files("Dungeon Props/Textures/1K/T_Crate_N.png"),
            ["Dungeon Props/Textures/1K/T_Crate_N.png"]
        );
        // An image with no role suffix is a BaseColor set of its own
        let mattress = images
            .find("Dungeon Props/Textures/mattress_01.png")
            .unwrap();
        assert_eq!(set_files("Dungeon Props/Textures/mattress_01.png").len(), 1);
        assert_eq!(images.layout(mattress), Layout::Role(Role::BaseColor));

        // T_Rock_N_DX is the Rock set's Normal, in DirectX whatever the pack says (D14)
        let rock_n = images.find("Dungeon Props/Rocks/T_Rock_N_DX.png").unwrap();
        assert_eq!(images.layout(rock_n), Layout::Role(Role::Normal));
        assert_eq!(
            images.convention(rock_n, Convention::OpenGl),
            Convention::DirectX
        );
        assert_eq!(pascal(images.set_base(images.set_of[rock_n])), "Rock");
        let rock = images.set_of[rock_n];
        assert_eq!(
            images.role_sources(rock, Role::MetallicSmoothness).len(),
            2,
            "R and M merge into one MetallicSmoothness"
        );
    }

    #[test]
    fn fbx_references_resolve_inside_the_pack() {
        // The fixture's absolute paths are another checkout's, so the relative ones resolve
        let (images, pillar) = infer("Dungeon Props", "Dungeon Props/Models/SM_Pillar.fbx");
        let stone = &pillar[0];
        assert_eq!(
            (stone.slot.as_str(), stone.name.as_str()),
            ("Stone", "Stone")
        );
        assert_eq!(
            textures(&images, stone),
            [
                (
                    Role::BaseColor,
                    paths(&["Dungeon Props/Textures/T_StoneFloor_D.png"]),
                    Via::Fbx
                ),
                (
                    Role::Normal,
                    paths(&["Dungeon Props/Textures/T_StoneFloor_N.png"]),
                    Via::Fbx
                ),
            ]
        );
        // Moss has no reference, and no image is named like it or the pillar in this pack
        assert_eq!(pillar[1].slot, "Moss");
        assert!(pillar[1].textures.is_empty());
        assert_eq!(pillar.len(), 2);

        // A reference by absolute path inside the library, and one found only by its name
        let mut images = pack_images("Dungeon Props");
        let absolute = root().join("Dungeon Props/Textures/T_Wall_N_DX.png");
        let facts = FbxFacts {
            slots: vec![Slot {
                name: "Wall".into(),
                textures: BTreeMap::from([
                    (
                        "normal".to_string(),
                        vec![TextureRef {
                            file: Some(absolute.display().to_string()),
                            relative: Some("nowhere/T_Wall_N_DX.png".into()),
                        }],
                    ),
                    (
                        "baseColor".to_string(),
                        vec![TextureRef {
                            file: Some("C:\\Art\\textures\\t_wall_d.png".into()),
                            relative: Some("..\\..\\t_wall_d.png".into()),
                        }],
                    ),
                    (
                        // Outside the pack: never found
                        "emissive".to_string(),
                        vec![TextureRef {
                            file: None,
                            relative: Some("../../../Kenney Crates/crate_wood.png".into()),
                        }],
                    ),
                ]),
            }],
            has_kept_mesh: true,
            ..FbxFacts::default()
        };
        let model = Model {
            item_path: "Dungeon Props/Models/SM_Wall.fbx",
            fbx_path: "Dungeon Props/Models/SM_Wall.fbx",
            facts: &facts,
        };
        let wall = materials(&mut images, root(), None, &model);
        assert_eq!(
            textures(&images, &wall[0]),
            [
                // By name, ignoring case and extension: the JPG
                (
                    Role::BaseColor,
                    paths(&["Dungeon Props/Textures/T_Wall_D.jpg"]),
                    Via::Fbx
                ),
                (
                    Role::Normal,
                    paths(&["Dungeon Props/Textures/T_Wall_N_DX.png"]),
                    Via::Fbx
                ),
            ]
        );
    }

    #[test]
    fn suffixes_find_the_slots_and_the_models_maps() {
        // By the slot's name: Iron's base color, and a Roughness and Metallic pair
        let (images, gate) = infer("Dungeon Props", "Dungeon Props/Models/SM_Gate.fbx");
        assert_eq!(
            textures(&images, &gate[0]),
            [
                (
                    Role::BaseColor,
                    paths(&["Dungeon Props/Textures/T_Iron_D.png"]),
                    Via::Suffix
                ),
                (
                    Role::MetallicSmoothness,
                    paths(&[
                        "Dungeon Props/Textures/T_Iron_M.png",
                        "Dungeon Props/Textures/T_Iron_R.png"
                    ]),
                    Via::Suffix
                ),
            ]
        );
        assert_eq!(gate[0].surface, Surface::Opaque);
        assert!(!gate[0].cutout_suggested);

        // By the model's name: a cut-out base color is a suggestion, and AORM splits
        let (images, guard) = infer("Dungeon Props", "Dungeon Props/Models/SK_Guard.fbx");
        let cloth = &guard[0];
        assert_eq!(cloth.slot, "Cloth");
        assert_eq!(
            textures(&images, cloth),
            [
                (
                    Role::BaseColor,
                    paths(&["Dungeon Props/Textures/T_Guard_D.png"]),
                    Via::Suffix
                ),
                (
                    Role::MetallicSmoothness,
                    paths(&["Dungeon Props/Textures/T_Guard_AORM.png"]),
                    Via::Suffix
                ),
                (
                    Role::Occlusion,
                    paths(&["Dungeon Props/Textures/T_Guard_AORM.png"]),
                    Via::Suffix
                ),
            ]
        );
        assert_eq!(
            cloth.surface,
            Surface::Opaque,
            "a suggestion exports opaque"
        );
        assert!(cloth.cutout_suggested);

        // A JPG base color
        let (images, stool) = infer("Dungeon Props", "Dungeon Props/Models/SM_Stool.fbx");
        assert_eq!(
            textures(&images, &stool[0]),
            [(
                Role::BaseColor,
                paths(&["Dungeon Props/Textures/T_Stool_D.jpg"]),
                Via::Suffix
            )]
        );
    }

    #[test]
    fn the_crate_binds_the_1k_normal_map_by_folder_on_every_run() {
        for _ in 0..20 {
            let (images, crate_) = infer("Dungeon Props", "Dungeon Props/Models/SM_Crate.fbx");
            assert_eq!(
                textures(&images, &crate_[0]),
                [(
                    Role::Normal,
                    paths(&["Dungeon Props/Textures/1K/T_Crate_N.png"]),
                    Via::Suffix
                )]
            );
        }
    }

    #[test]
    fn the_models_own_folder_wins_then_the_nearest() {
        assert!(
            folder_rank("Kit/Models", "Kit/Models") < folder_rank("Kit/Models", "Kit/Textures")
        );
        assert!(
            folder_rank("Kit/Models", "Kit/Textures")
                < folder_rank("Kit/Models", "Kit/Textures/1K")
        );
        assert!(
            folder_rank("Kit/Models", "Kit/Textures/1K")
                < folder_rank("Kit/Models", "Kit/Textures/4K")
        );
        assert_eq!(folder_distance("", "a/b"), 2);
        assert_eq!(folder_distance("a/b", "a/c/d"), 3);
    }

    #[test]
    fn an_image_with_the_models_stem_is_the_added_slots_base_color() {
        let (images, boulder) = infer("Dungeon Props", "Dungeon Props/Models/SM_Boulder.fbx");
        assert_eq!(boulder.len(), 1);
        let added = &boulder[0];
        assert_eq!(
            (added.slot.as_str(), added.name.as_str(), added.added),
            ("OpenDAMDefault", "OpenDAMDefault", true)
        );
        assert_eq!(
            textures(&images, added),
            [(
                Role::BaseColor,
                paths(&["Dungeon Props/Models/SM_Boulder.png"]),
                Via::Stem
            )]
        );
        // Scans/T_Boulder_D.png matches the model's name, but lies in another pack
    }

    #[test]
    fn the_added_slot_follows_the_kept_slots() {
        let (_, lamp) = infer("Dungeon Props", "Dungeon Props/Models/SM_Lamp.fbx");
        let slots: Vec<(&str, bool)> = lamp.iter().map(|m| (m.slot.as_str(), m.added)).collect();
        assert_eq!(
            slots,
            [("DefaultMaterial", false), ("OpenDAMDefault", true)]
        );
        let (_, shelf) = infer("Dungeon Props", "Dungeon Props/Models/SM_Shelf.fbx");
        let slots: Vec<(&str, bool)> = shelf.iter().map(|m| (m.slot.as_str(), m.added)).collect();
        assert_eq!(
            slots,
            [("OpenDAMDefault", false), ("OpenDAMDefault2", true)]
        );
        let (_, stool) = infer("Dungeon Props", "Dungeon Props/Models/SM_Stool.fbx");
        assert_eq!(
            (stool[0].slot.as_str(), stool[0].name.as_str()),
            ("Material", "Material")
        );
    }

    #[test]
    fn a_unity_material_wins_over_suffixes_and_says_cutout() {
        let (images, log) = infer("Unity Moss", "Unity Moss/Models/SM_Log.fbx");
        let (bark, leaves) = (&log[0], &log[1]);
        // The .fbx.meta remap finds M_LogBark.mat, not T_Bark_D.png beside the model
        assert_eq!(
            textures(&images, bark),
            [
                (
                    Role::BaseColor,
                    paths(&["Unity Moss/Textures/T_Bark_Albedo.png"]),
                    Via::Unity
                ),
                (
                    Role::Normal,
                    paths(&["Unity Moss/Textures/T_Bark_Normal.png"]),
                    Via::Unity
                ),
            ]
        );
        assert_eq!(bark.surface, Surface::Opaque);
        // Leaves.mat is found by the slot's name, clips alpha, and names an image with no suffix
        assert_eq!(
            textures(&images, leaves),
            [(
                Role::BaseColor,
                paths(&["Unity Moss/Textures/T_LeafAtlas.png"]),
                Via::Unity
            )]
        );
        assert_eq!(leaves.surface, Surface::Cutout);
        assert!(!leaves.cutout_suggested, "the material said it");
    }

    #[test]
    fn alpha_below_half_is_a_cut_and_half_is_not() {
        use image::{ImageBuffer, Rgb, Rgba};
        let dir = tempfile::tempdir().unwrap();
        let save8 = |name: &str, alpha: u8| {
            let mut img = ImageBuffer::from_pixel(2, 2, Rgba([90u8, 90, 90, 255]));
            img.put_pixel(1, 1, Rgba([90, 90, 90, alpha]));
            img.save(dir.path().join(name)).unwrap();
        };
        let save16 = |name: &str, alpha: u16| {
            let mut img = ImageBuffer::from_pixel(2, 2, Rgba([9000u16, 9000, 9000, 65535]));
            img.put_pixel(0, 1, Rgba([9000, 9000, 9000, alpha]));
            img.save(dir.path().join(name)).unwrap();
        };
        save8("a127.png", 127);
        save8("a128.png", 128);
        save8("a254.png", 254);
        save16("w32767.png", 32767);
        save16("w32768.png", 32768);
        ImageBuffer::from_pixel(2, 2, Rgb([90u8, 90, 90]))
            .save(dir.path().join("rgb.png"))
            .unwrap();
        let names = [
            "a127.png",
            "a128.png",
            "a254.png",
            "w32767.png",
            "w32768.png",
            "rgb.png",
        ];
        let mut images = PackImages::new(
            names
                .iter()
                .map(|n| Image {
                    id: n.to_string(),
                    path: n.to_string(),
                })
                .collect(),
        );
        let cut = |images: &mut PackImages, name: &str| {
            let i = images.find(name).unwrap();
            images.has_cut_alpha(dir.path(), i)
        };
        // 8 bit: 127 is below 0.5, 128 is not
        assert!(cut(&mut images, "a127.png"));
        assert!(!cut(&mut images, "a128.png"));
        assert!(!cut(&mut images, "a254.png"));
        // 16 bit: the same boundary at its own depth
        assert!(cut(&mut images, "w32767.png"));
        assert!(!cut(&mut images, "w32768.png"));
        // No alpha channel, no cut
        assert!(!cut(&mut images, "rgb.png"));
        // Grey with alpha reads its own alpha channel
        let mut la = ImageBuffer::from_pixel(2, 2, image::LumaA([90u8, 255]));
        la.put_pixel(0, 0, image::LumaA([90, 127]));
        la.save(dir.path().join("la127.png")).unwrap();
        assert!(read_cut_alpha(&dir.path().join("la127.png")));
    }

    /// The materials of a model at `fbx_path` in `pack` whose one slot `slot` references
    /// `references`, by type, with its relative path.
    fn infer_slot(
        pack: &str,
        fbx_path: &str,
        slot: &str,
        references: &[(&str, &str)],
    ) -> (PackImages, Vec<Material>) {
        let mut images = pack_images(pack);
        let facts = FbxFacts {
            slots: vec![Slot {
                name: slot.into(),
                textures: references
                    .iter()
                    .map(|(kind, relative)| {
                        (
                            kind.to_string(),
                            vec![TextureRef {
                                file: None,
                                relative: Some(relative.to_string()),
                            }],
                        )
                    })
                    .collect(),
            }],
            has_kept_mesh: true,
            ..FbxFacts::default()
        };
        let unity = pack
            .starts_with("Unity Moss")
            .then(|| UnityPack::read(root(), &pack_files(pack)));
        let model = Model {
            item_path: fbx_path,
            fbx_path,
            facts: &facts,
        };
        let materials = materials(&mut images, root(), unity.as_ref(), &model);
        (images, materials)
    }

    #[test]
    fn an_image_referenced_as_two_types_gives_the_first_types_role_only() {
        // Blender links Emission Color to the base color image: the image is one BaseColor, and
        // no Emission binding is left without a file that gives it
        let (images, screen) = infer_slot(
            "Dungeon Props",
            "Dungeon Props/Models/SM_Screen.fbx",
            "Screen",
            &[
                ("baseColor", "../Textures/mattress_01.png"),
                ("emissive", "../Textures/mattress_01.png"),
            ],
        );
        assert_eq!(
            textures(&images, &screen[0]),
            [(
                Role::BaseColor,
                paths(&["Dungeon Props/Textures/mattress_01.png"]),
                Via::Fbx
            )]
        );
    }

    #[test]
    fn each_role_falls_through_to_the_next_source_on_its_own() {
        // The FBX names only a base color: it wins over the slot's suffixed T_StoneFloor_D,
        // and the normal map still comes from the suffix lookup
        let (images, floor) = infer_slot(
            "Dungeon Props",
            "Dungeon Props/Models/SM_Floor.fbx",
            "StoneFloor",
            &[("baseColor", "../Textures/T_Wall_D.jpg")],
        );
        assert_eq!(
            textures(&images, &floor[0]),
            [
                (
                    Role::BaseColor,
                    paths(&["Dungeon Props/Textures/T_Wall_D.jpg"]),
                    Via::Fbx
                ),
                (
                    Role::Normal,
                    paths(&["Dungeon Props/Textures/T_StoneFloor_N.png"]),
                    Via::Suffix
                ),
            ]
        );

        // The FBX's reference wins over the Unity material's base color, and the material
        // still gives the normal map
        let (images, log) = infer_slot(
            "Unity Moss",
            "Unity Moss/Models/SM_Log.fbx",
            "Bark",
            &[("baseColor", "T_Bark_D.png")],
        );
        assert_eq!(
            textures(&images, &log[0]),
            [
                (
                    Role::BaseColor,
                    paths(&["Unity Moss/Models/T_Bark_D.png"]),
                    Via::Fbx
                ),
                (
                    Role::Normal,
                    paths(&["Unity Moss/Textures/T_Bark_Normal.png"]),
                    Via::Unity
                ),
            ]
        );

        // A suffixed base color wins over SM_Boulder.png, the image with the model's stem
        let (images, boulder) = infer_slot(
            "Dungeon Props",
            "Dungeon Props/Models/SM_Boulder.fbx",
            "Iron",
            &[],
        );
        assert_eq!(
            textures(&images, &boulder[0])[0],
            (
                Role::BaseColor,
                paths(&["Dungeon Props/Textures/T_Iron_D.png"]),
                Via::Suffix
            )
        );
    }

    #[test]
    fn file_reads_are_kept_while_the_file_keeps_its_size_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("T_Rock_D.png");
        std::fs::write(&file, b"one").unwrap();
        let memo = FileMemo::new();
        let reads = std::cell::Cell::new(0);
        let read = |_: &Path| {
            reads.set(reads.get() + 1);
            reads.get()
        };
        assert_eq!(memo.get(&file, read), Some(1));
        assert_eq!(memo.get(&file, read), Some(1), "unchanged: not read again");
        std::fs::write(&file, b"three").unwrap();
        assert_eq!(memo.get(&file, read), Some(2), "a new size is read again");
        let handle = std::fs::File::options().write(true).open(&file).unwrap();
        handle.set_modified(SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(memo.get(&file, read), Some(3), "a new mtime too");
        assert_eq!(memo.get(&dir.path().join("gone.png"), read), None);
        assert_eq!(reads.get(), 3);
    }

    #[test]
    fn set_names_follow_their_users() {
        let images = pack_images("Dungeon Props");
        let floor = images.set_of[images
            .find("Dungeon Props/Textures/T_StoneFloor_D.png")
            .unwrap()];
        let names = ["Totem".to_string(), "Pillar".to_string()];
        let one = |_: usize, _: Role| 1;
        // No user, or two: its stem
        assert_eq!(set_name(&images, floor, &[], &names, one), "StoneFloor");
        let both = [
            (0, "Stone".to_string(), vec![Role::BaseColor]),
            (1, "Stone".to_string(), vec![Role::BaseColor]),
        ];
        assert_eq!(set_name(&images, floor, &both, &names, one), "StoneFloor");
        // One user: the model's name, with the material's when it has several sets in the role
        let pillar = [(1, "Stone".to_string(), vec![Role::BaseColor, Role::Normal])];
        assert_eq!(set_name(&images, floor, &pillar, &names, one), "Pillar");
        let crowded = |_: usize, role: Role| if role == Role::Normal { 2 } else { 1 };
        assert_eq!(
            set_name(&images, floor, &pillar, &names, crowded),
            "PillarStone"
        );
    }
}
