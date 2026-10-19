//! The structure pass: a library's folder tree, its packs, its support files, its format groups
//! with their bundled previews, and its 2D stacks, derived from the paths it has indexed
//! (docs/plans/smarter-big-collections/plan.md, "The structure pass"). `derive` is a pure function
//! over the paths, their sizes and what the license files say, and the pass writes only the folder rows and `assets.support_role`,
//! `assets.group_id` and `assets.bundled_preview` values that differ from it, and the metadata of
//! the groups that form or grow (`apply_unions`). Every index run ends with the pass
//! (`watcher::index_paths`), and so does every command that changes the paths, through
//! `rebuild_library`. Nothing else writes `folders`, `support_role`, `group_id` or
//! `bundled_preview`.

use serde::{Deserialize, Serialize};
use sqlx::{Pool, Sqlite};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Bound;
use std::path::Path;
use std::time::Instant;

use crate::db::{Database, LibraryHandle};
use crate::models::AssetKind;

/// Rows per write transaction, as in `watcher.rs`.
const BATCH: usize = 200;

/// File stems (lowercase) that mark the folder holding them as a pack, on a doc or an image.
/// A Readme marks nothing: `Archive/` and `Early access/` each hold one above several packs.
const MARKER_STEMS: [&str; 4] = ["license", "licence", "preview", "sample"];
/// Image stems (lowercase) that make a pack's cover images.
const COVER_STEMS: [&str; 2] = ["preview", "sample"];
/// Doc stems (lowercase) of a pack's license file, whose header names the pack.
const LICENSE_STEMS: [&str; 2] = ["license", "licence"];
const DOC_EXTENSIONS: [&str; 3] = ["txt", "md", "pdf"];
/// How much of a license file the pass reads for its header and license.
const HEADER_BYTES: u64 = 4096;
/// The version of `parse_license`. A license file read by an older one is read again once, so a
/// library indexed before a field existed gains it.
const LICENSE_PARSER: u32 = 1;
/// The formats a group picks its main item by (plan D12), best first. Every other format ranks
/// after them.
const MAIN_FORMATS: [&str; 5] = ["glb", "gltf", "fbx", "blend", "obj"];
/// The formats a folder of model copies is named after, ignoring case and an optional ` format`
/// or ` files` (`GLB format`, `FBX`, `obj files`). `fbx(unity)` and `fbx (unity)` are FBX
/// folders too.
const FORMAT_FOLDERS: [&str; 11] = [
    "glb", "gltf", "fbx", "obj", "blend", "dae", "stl", "3ds", "usd", "usdz", "ply",
];
/// The names (lowercase) of a folder of model renders, such as Kenney's `Previews/`.
const RENDER_FOLDERS: [&str; 3] = ["previews", "preview", "renders"];
/// The names (lowercase) of a folder of a sprite's copies at its default scale (plan C13).
const DEFAULT_SCALE_FOLDERS: [&str; 3] = ["default", "default size", "1x"];
/// The names (lowercase) of a folder of a sprite's copies at a larger scale.
const LARGER_SCALE_FOLDERS: [&str; 8] = ["double", "large", "retina", "hd", "2x", "3x", "4x", "x2"];
/// The names (lowercase) of a folder of a sprite's copies in one format.
const IMAGE_FORMAT_FOLDERS: [&str; 4] = ["png", "svg", "vector", "vectors"];
/// The endings (lowercase) of a folder name that make it a larger scale of the folder named
/// without them, such as `Sprites X2` beside `Sprites`.
const LARGER_SCALE_SUFFIXES: [&str; 4] = [" x2", " hd", " (2×)", " (2x)"];

/// One folder as the pass derives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// `None` for the root
    pub parent_path: Option<String>,
    pub name: String,
    pub is_pack: bool,
    /// Grid items with a visible file below the folder, at any depth: a format group counts
    /// once, and support files not at all.
    pub item_count: i64,
    /// Support files below the folder, at any depth
    pub hidden_count: i64,
    /// Format groups with a member below the folder and their main item outside it, such as a
    /// kit's models under `FBX format/`. A search for the folder finds those groups through their
    /// members, and finds every other item below it by the item's own path.
    pub split_groups: i64,
    /// A pack's info, stored as `folders.pack_json`; `None` for a folder that is no pack
    pub pack: Option<PackInfo>,
}

/// What the pass derives for one pack (plan C9).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackInfo {
    /// The name shown: the license header's, else the folder's
    pub name: String,
    /// The cover's path: the first `Preview*` cover image, else the first `Sample*` one, else the
    /// pack's first image, else its first visible file (a model, whose thumbnail shows), each by
    /// path
    pub cover: Option<String>,
    /// Every cover image of the pack (its `Preview*` and `Sample*` images), by path
    #[serde(default)]
    pub gallery: Vec<String>,
    pub license: Option<LicenseFile>,
    /// How many files of each kind (`AssetKind::as_str`) lie below the pack, at any depth,
    /// support files and group members included
    #[serde(default)]
    pub kinds: BTreeMap<String, i64>,
    /// The total size in bytes of those files
    #[serde(default)]
    pub size_bytes: i64,
}

/// A pack's license file: its first `License*` doc by path, directly inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseFile {
    pub path: String,
    /// What the pass read from the file (`parse_license`)
    #[serde(flatten)]
    pub text: LicenseText,
    /// The `LICENSE_PARSER` that read it, and the file's size and mtime then, so a rerun reads
    /// only a changed file. `derive` leaves them 0, and `rebuild` fills them from the file's row
    /// once it has read the file.
    #[serde(default)]
    pub parser: u32,
    #[serde(default)]
    pub size_bytes: i64,
    #[serde(default)]
    pub modified_at: i64,
}

/// What a license file says (`parse_license`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseText {
    #[serde(default)]
    pub kind: LicenseKind,
    /// Its Kenney header, if it has one
    #[serde(default)]
    pub header: Option<PackHeader>,
}

/// The license a license file names (`license_kind`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LicenseKind {
    #[serde(rename = "CC0")]
    Cc0,
    #[serde(rename = "CC-BY")]
    CcBy,
    #[serde(rename = "CC-BY-SA")]
    CcBySa,
    #[serde(rename = "MIT")]
    Mit,
    #[serde(rename = "Apache-2.0")]
    Apache2,
    #[serde(rename = "OFL")]
    Ofl,
    /// Any other license, or text that names none
    #[default]
    #[serde(rename = "unknown")]
    Unknown,
}

/// A Kenney license header (`kenney_header`): `Car Kit (3.1)`, then `Created/distributed by
/// Kenney (www.kenney.nl)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackHeader {
    /// The first line without its version: `Car Kit`
    pub name: String,
    /// The version from the first line's parentheses: `3.1`
    pub version: Option<String>,
    /// The byline's name without what follows it in parentheses: `Kenney`
    pub author: Option<String>,
}

/// Why a file is a pack's support file, hidden from the grid unless "Show hidden files" is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportRole {
    /// A doc (`txt`, `md`, `pdf`): a license, a readme
    Doc,
    /// A `Preview*` or `Sample*` image
    Cover,
    /// A model's render, its group's bundled preview (`model_previews`)
    ModelPreview,
}

impl SupportRole {
    pub fn as_str(self) -> &'static str {
        match self {
            SupportRole::Doc => "doc",
            SupportRole::Cover => "cover",
            SupportRole::ModelPreview => "model_preview",
        }
    }
}

/// What the pass derives for one library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Structure {
    /// Every folder by its path relative to the library root. The root is `""`.
    pub folders: BTreeMap<String, Folder>,
    /// The support role of each input path, in input order
    pub roles: Vec<Option<SupportRole>>,
    /// The format group of each input path, in input order: the input index of its main item,
    /// itself for the main item, or `None` for a file on its own
    pub groups: Vec<Option<usize>>,
    /// The bundled preview of each input path, in input order: the input index of its render,
    /// for a main item or a file on its own whose model has one, and `None` for every other file
    pub bundled_previews: Vec<Option<usize>>,
}

/// The folder holding `path` (`""` for the root) and the file's name.
fn split_dir(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// Every folder above `dir`, from the root down, excluding `dir` itself.
fn ancestors(dir: &str) -> impl Iterator<Item = &str> {
    std::iter::once("").filter(move |_| !dir.is_empty()).chain(
        dir.match_indices('/')
            .map(move |(end, _)| &dir[..end])
            .filter(|a| !a.is_empty()),
    )
}

/// A file's lowercase stem and extension, and whether it is a doc, an image or a model.
struct FileKind {
    stem: String,
    ext: String,
    is_doc: bool,
    is_image: bool,
    is_model: bool,
}

impl FileKind {
    fn of(name: &str) -> Self {
        let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
        let ext = ext.to_lowercase();
        let kind = AssetKind::from_extension(&ext);
        FileKind {
            stem: stem.to_lowercase(),
            is_doc: DOC_EXTENSIONS.contains(&ext.as_str()),
            is_image: matches!(kind, AssetKind::Texture | AssetKind::Image),
            is_model: kind == AssetKind::Model3d,
            ext,
        }
    }

    fn stem_starts_with(&self, stems: &[&str]) -> bool {
        stems.iter().any(|s| self.stem.starts_with(s))
    }

    fn is_marker(&self) -> bool {
        (self.is_doc || self.is_image) && self.stem_starts_with(&MARKER_STEMS)
    }

    fn is_license(&self) -> bool {
        self.is_doc && self.stem_starts_with(&LICENSE_STEMS)
    }

    fn support_role(&self) -> Option<SupportRole> {
        if self.is_doc {
            Some(SupportRole::Doc)
        } else if self.is_image && self.stem_starts_with(&COVER_STEMS) {
            Some(SupportRole::Cover)
        } else {
            None
        }
    }
}

/// The structure of a library holding the files at `paths` (relative, `/`-separated):
///
/// 1. **Folders:** the root, plus every folder with at least one file below it.
/// 2. **Packs** (plan C8): a folder is marked when a doc or an image directly inside it has a
///    stem starting with `license`, `licence`, `preview` or `sample`, ignoring case. The
///    outermost marked folder is the pack. Then the sibling rule: a folder that isn't inside a
///    pack and holds no pack is also one when its parent has at least 2 pack children and packs
///    are more than half of its child folders.
/// 3. **Support roles** (C10): in a pack's own folder, or in a marked folder inside a pack, docs
///    are `Doc` and `Preview*`/`Sample*` images are `Cover`.
/// 4. **Format groups** (C11, `format_groups`): one model shipped in several formats, and the
///    copies of a file under format folders, form one grid item led by its main item.
/// 5. **Bundled previews** (C12, `model_previews`): a render of a model is a `ModelPreview`
///    support file, and the model's main item records it.
/// 6. **2D stacks** (C13, `stacks`): a sprite's scale and format copies (`Default/`, `Double/`,
///    `Vector/`) form a group too, led by its PNG at the default scale.
/// 7. **Counts:** each folder counts the grid items with a visible file below it, a group once,
///    and the support files below it.
/// 8. **Pack info** (C9): each pack's cover, gallery and license file, which `read_license`
///    reads, its name, which is the license's header name or else the folder's, and the files
///    below it by kind with their total size.
///
/// Each file comes with its size in bytes.
pub fn derive<'a>(
    files: impl IntoIterator<Item = (&'a str, i64)>,
    mut read_license: impl FnMut(&str) -> LicenseText,
) -> Structure {
    let (paths, sizes): (Vec<&str>, Vec<i64>) = files.into_iter().unzip();
    let kinds: Vec<FileKind> = paths.iter().map(|p| FileKind::of(split_dir(p).1)).collect();

    let mut folders: BTreeMap<String, Folder> = BTreeMap::new();
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut marked: HashSet<&str> = HashSet::new();
    folders.insert(String::new(), folder(None, ""));
    for (path, kind) in paths.iter().zip(&kinds) {
        let (dir, _) = split_dir(path);
        if kind.is_marker() {
            marked.insert(dir);
        }
        for (end, _) in path.match_indices('/') {
            let dir = &path[..end];
            if dir.is_empty() || folders.contains_key(dir) {
                continue;
            }
            let (parent, name) = split_dir(dir);
            folders.insert(dir.to_string(), folder(Some(parent), name));
            children.entry(parent).or_default().push(dir);
        }
    }

    // The outermost marked folders, then the sibling rule's packs.
    let mut packs: BTreeSet<&str> = marked
        .iter()
        .copied()
        .filter(|dir| !ancestors(dir).any(|a| marked.contains(a)))
        .collect();
    let mut by_siblings: Vec<&str> = Vec::new();
    for (parent, kids) in &children {
        let pack_kids = kids.iter().filter(|k| packs.contains(*k)).count();
        if pack_kids < 2 || pack_kids * 2 <= kids.len() {
            continue;
        }
        if in_pack(&packs, parent) {
            continue;
        }
        by_siblings.extend(
            kids.iter()
                .filter(|k| !packs.contains(*k) && !holds_pack(&packs, k)),
        );
    }
    packs.extend(by_siblings);

    let mut roles: Vec<Option<SupportRole>> = paths
        .iter()
        .zip(&kinds)
        .map(|(path, kind)| {
            let (dir, _) = split_dir(path);
            (packs.contains(dir) || (marked.contains(dir) && in_pack(&packs, dir)))
                .then(|| kind.support_role())
                .flatten()
        })
        .collect();
    let renders = model_previews(&paths, &kinds, &mut roles);
    let mut groups = format_groups(&paths, &kinds, &roles);
    stacks(&paths, &kinds, &roles, &mut groups);
    let bundled_previews = bundled_previews(&paths, &groups, &renders);

    // A file on its own counts toward the folders above it. A group counts once toward each
    // folder above any of its files, so every format folder of a kit counts all its models.
    let mut group_folders: HashMap<usize, Vec<&str>> = HashMap::new();
    for (i, path) in paths.iter().enumerate() {
        if roles[i].is_some() {
            for dir in ancestors(path) {
                folders
                    .get_mut(dir)
                    .expect("every ancestor has a row")
                    .hidden_count += 1;
            }
        } else if let Some(main) = groups[i] {
            group_folders
                .entry(main)
                .or_default()
                .extend(ancestors(path));
        } else {
            for dir in ancestors(path) {
                folders
                    .get_mut(dir)
                    .expect("every ancestor has a row")
                    .item_count += 1;
            }
        }
    }
    for (main, mut dirs) in group_folders {
        dirs.sort_unstable();
        dirs.dedup();
        let main_dirs: HashSet<&str> = ancestors(paths[main]).collect();
        for dir in dirs {
            let folder = folders.get_mut(dir).expect("every ancestor has a row");
            folder.item_count += 1;
            if !main_dirs.contains(dir) {
                folder.split_groups += 1;
            }
        }
    }
    let mut info = pack_files(&packs, &paths, &kinds, &roles, &groups);
    let mut contents = pack_contents(&packs, &paths, &kinds, &sizes);
    for pack in packs {
        let folder = folders.get_mut(pack).expect("a pack has a row");
        let files = info.remove(pack).unwrap_or_default();
        let license = files.license.map(|path| LicenseFile {
            text: read_license(path),
            path: path.to_string(),
            parser: 0,
            size_bytes: 0,
            modified_at: 0,
        });
        let (kinds, size_bytes) = contents.remove(pack).unwrap_or_default();
        folder.is_pack = true;
        folder.pack = Some(PackInfo {
            name: license
                .as_ref()
                .and_then(|l| Some(l.text.header.as_ref()?.name.clone()))
                .unwrap_or_else(|| folder.name.clone()),
            cover: files.cover().map(str::to_string),
            gallery: files.gallery.iter().map(|p| p.to_string()).collect(),
            license,
            kinds,
            size_bytes,
        });
    }
    Structure {
        folders,
        roles,
        groups,
        bundled_previews,
    }
}

/// The renders among `paths` (plan C12): an image in a folder named `Previews`, `Preview` or
/// `Renders`, ignoring case, renders each model below that folder's parent whose lowercase stem
/// equals its own. Each render becomes a `ModelPreview` support file in `roles`, and is returned
/// with the models it renders, by input index. An image that already has a role, or renders no
/// model, stays as it is. So does one the webview can't display itself (`.dds`, `.tga`): its
/// card would show nothing whenever it has no thumbnail, while the model it renders lost its own
/// place on the preview queue.
fn model_previews(
    paths: &[&str],
    kinds: &[FileKind],
    roles: &mut [Option<SupportRole>],
) -> Vec<(usize, Vec<usize>)> {
    let mut models: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, kind) in kinds.iter().enumerate() {
        if kind.is_model {
            models.entry(kind.stem.as_str()).or_default().push(i);
        }
    }
    let mut renders = Vec::new();
    for (i, (path, kind)) in paths.iter().zip(kinds).enumerate() {
        if !kind.is_image
            || roles[i].is_some()
            || !crate::preview::is_webview_displayable(&kind.ext)
        {
            continue;
        }
        let (dir, _) = split_dir(path);
        let (parent, folder) = split_dir(dir);
        if dir.is_empty() || !RENDER_FOLDERS.contains(&folder.to_ascii_lowercase().as_str()) {
            continue;
        }
        let below = format!("{parent}/");
        let rendered: Vec<usize> = models
            .get(kind.stem.as_str())
            .into_iter()
            .flatten()
            .copied()
            .filter(|&m| parent.is_empty() || paths[m].starts_with(&below))
            .collect();
        if !rendered.is_empty() {
            roles[i] = Some(SupportRole::ModelPreview);
            renders.push((i, rendered));
        }
    }
    renders
}

/// `Structure::bundled_previews` for `renders` as `model_previews` returns them: each rendered
/// model's main item, or the model itself when it is on its own, records its render. A render of
/// models in several groups is each group's preview, and an item rendered more than once takes
/// the render whose path sorts first ignoring case, as `format_groups` breaks ties.
fn bundled_previews(
    paths: &[&str],
    groups: &[Option<usize>],
    renders: &[(usize, Vec<usize>)],
) -> Vec<Option<usize>> {
    let key = |i: usize| (paths[i].to_ascii_lowercase(), paths[i]);
    let mut bundled: Vec<Option<usize>> = vec![None; paths.len()];
    for (render, models) in renders {
        for &model in models {
            let item = groups[model].unwrap_or(model);
            if bundled[item].is_none_or(|b| key(*render) < key(b)) {
                bundled[item] = Some(*render);
            }
        }
    }
    bundled
}

/// The files of one pack that its info is chosen from, each the first by path.
#[derive(Default)]
struct PackFiles<'a> {
    preview: Option<&'a str>,
    sample: Option<&'a str>,
    image: Option<&'a str>,
    visible: Option<&'a str>,
    license: Option<&'a str>,
    /// Every cover image, by path
    gallery: BTreeSet<&'a str>,
}

impl<'a> PackFiles<'a> {
    fn cover(&self) -> Option<&'a str> {
        self.preview.or(self.sample).or(self.image).or(self.visible)
    }
}

/// Keeps the lesser of `slot` and `path`.
fn first<'a>(slot: &mut Option<&'a str>, path: &'a str) {
    if slot.is_none_or(|p| path < p) {
        *slot = Some(path);
    }
}

/// The files each pack's info is chosen from. A file belongs to the innermost pack above it.
fn pack_files<'a>(
    packs: &BTreeSet<&str>,
    paths: &[&'a str],
    kinds: &[FileKind],
    roles: &[Option<SupportRole>],
    groups: &[Option<usize>],
) -> HashMap<String, PackFiles<'a>> {
    let mut files: HashMap<String, PackFiles> = HashMap::new();
    for (i, ((path, kind), role)) in paths.iter().zip(kinds).zip(roles).enumerate() {
        let (dir, _) = split_dir(path);
        let Some(pack) = std::iter::once(dir)
            .chain(ancestors(dir).collect::<Vec<_>>().into_iter().rev())
            .find(|d| packs.contains(d))
        else {
            continue;
        };
        let entry = files.entry(pack.to_string()).or_default();
        // A grid item: a file on its own or a group's main item, never another member
        if role.is_none() && groups[i].is_none_or(|main| main == i) {
            first(&mut entry.visible, path);
        }
        if kind.is_image {
            first(&mut entry.image, path);
            if *role == Some(SupportRole::Cover) {
                entry.gallery.insert(path);
                if kind.stem.starts_with("preview") {
                    first(&mut entry.preview, path);
                } else {
                    first(&mut entry.sample, path);
                }
            }
        }
        if dir == pack && kind.is_license() {
            first(&mut entry.license, path);
        }
    }
    files
}

/// The files of each pack by kind, and their total size: every file below the pack, at any depth,
/// so an outer pack counts the files of a pack nested in it too.
fn pack_contents(
    packs: &BTreeSet<&str>,
    paths: &[&str],
    kinds: &[FileKind],
    sizes: &[i64],
) -> HashMap<String, (BTreeMap<String, i64>, i64)> {
    let mut contents: HashMap<String, (BTreeMap<String, i64>, i64)> = HashMap::new();
    for ((path, kind), size) in paths.iter().zip(kinds).zip(sizes) {
        let (dir, _) = split_dir(path);
        for pack in ancestors(dir)
            .chain(std::iter::once(dir))
            .filter(|d| packs.contains(d))
        {
            let (by_kind, total) = contents.entry(pack.to_string()).or_default();
            let name = AssetKind::from_extension(&kind.ext).as_str();
            *by_kind.entry(name.to_string()).or_default() += 1;
            *total += size;
        }
    }
    contents
}

/// What a license file's text says: the license it names (`license_kind`) and its Kenney header
/// (`kenney_header`).
pub fn parse_license(text: &str) -> LicenseText {
    LicenseText {
        kind: license_kind(text),
        header: kenney_header(text),
    }
}

/// `text` in lowercase, with each run of whitespace as one space.
fn squash(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A Kenney license header: its first line (`Car Kit (3.1)`) gives the name, without the version
/// in parentheses, when the next line names Kenney as the author (`Created/distributed by Kenney
/// (www.kenney.nl)` or `by Kenney Vleugels (Kenney.nl)`), which gives the author. `None` for any
/// other text, such as an MIT or CC license.
pub fn kenney_header(text: &str) -> Option<PackHeader> {
    let mut lines = text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    let title = lines.next()?;
    let byline = lines
        .next()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let lower = byline.to_lowercase();
    let by = ["created/distributed by ", "by "]
        .iter()
        .find(|by| lower.starts_with(*by))?;
    let author = byline.get(by.len()..)?;
    let author = author.split(" (").next().unwrap_or(author).trim();
    if !author.to_lowercase().starts_with("kenney") {
        return None;
    }
    // Drops a version such as ` (3.1)` or ` (1.5A)`, wherever it sits.
    let mut name = title.to_string();
    let mut version = None;
    if let Some(start) = name.find(" (") {
        let rest = &name[start + 2..];
        if rest.starts_with(|c: char| c.is_ascii_digit()) {
            if let Some(len) = rest.find(')') {
                version = Some(rest[..len].trim().to_string());
                name.replace_range(start..start + 2 + len + 1, "");
            }
        }
    }
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty()).then_some(PackHeader {
        name,
        version,
        author: Some(author.to_string()),
    })
}

/// How each name or URL of a license is told apart from its neighbors: given the text after it,
/// the license it names, or `None` when it names another (`CC BY-NC` after `cc by`).
type Classify = fn(&str) -> Option<LicenseKind>;

/// The names and URLs `license_kind` looks for, in lowercase with single spaces.
const LICENSE_NAMES: [(&str, Classify); 16] = [
    ("creativecommons.org/publicdomain/zero", |_| {
        Some(LicenseKind::Cc0)
    }),
    ("cc0", |_| Some(LicenseKind::Cc0)),
    ("creative commons zero", |_| Some(LicenseKind::Cc0)),
    ("creativecommons.org/licenses/by", |rest| {
        if rest.starts_with('/') {
            Some(LicenseKind::CcBy)
        } else if rest.starts_with("-sa/") {
            Some(LicenseKind::CcBySa)
        } else {
            None
        }
    }),
    ("cc-by", cc_by),
    ("cc by", cc_by),
    // `license_kind` turns it into another license when the text names NonCommercial or
    // NoDerivatives anywhere.
    ("creative commons attribution", |rest| {
        if compact(rest).starts_with("sharealike") {
            Some(LicenseKind::CcBySa)
        } else {
            Some(LicenseKind::CcBy)
        }
    }),
    ("opensource.org/licenses/mit", |_| Some(LicenseKind::Mit)),
    ("mit license", |_| Some(LicenseKind::Mit)),
    // The MIT text itself, which many license files hold without its title
    ("permission is hereby granted, free of charge", |_| {
        Some(LicenseKind::Mit)
    }),
    ("apache.org/licenses/license-2.0", |_| {
        Some(LicenseKind::Apache2)
    }),
    ("apache-2.0", |_| Some(LicenseKind::Apache2)),
    ("apache license", |rest| {
        let rest = rest.trim_start_matches([',', ' ', '-']);
        let rest = rest.strip_prefix("version ").unwrap_or(rest);
        rest.starts_with('2').then_some(LicenseKind::Apache2)
    }),
    ("open font license", |_| Some(LicenseKind::Ofl)),
    ("scripts.sil.org/ofl", |_| Some(LicenseKind::Ofl)),
    ("openfontlicense.org", |_| Some(LicenseKind::Ofl)),
];

/// `cc-by` or `cc by`, given the text after it: `-sa` makes it CC-BY-SA, and `-nc` or `-nd`, or
/// a letter (`cc bypass`), some other text.
fn cc_by(rest: &str) -> Option<LicenseKind> {
    let rest = rest.trim_start_matches(['-', ' ']);
    if rest.starts_with("sa") && !rest[2..].starts_with(|c: char| c.is_ascii_alphabetic()) {
        Some(LicenseKind::CcBySa)
    } else if rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
        None
    } else {
        Some(LicenseKind::CcBy)
    }
}

/// `text` without spaces, hyphens, commas and parentheses, so a license's wordings compare alike:
/// `Non-Commercial`, `Non Commercial` and `NonCommercial`, or `by-nc` and `BY NC`.
fn compact(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, ' ' | '-' | ',' | '(' | ')'))
        .collect()
}

/// What a Creative Commons license adds that makes it neither CC-BY nor CC-BY-SA, as `compact`
/// spells it: NonCommercial (`noncommercial`, `by-nc`) or NoDerivatives (`noderiv`, `by-nd`).
const CC_RESTRICTIONS: [&str; 4] = ["noncommercial", "noderiv", "bync", "bynd"];

/// The license `text` names first, by its name or its URL, ignoring case: CC0, CC-BY, CC-BY-SA,
/// MIT, Apache-2.0 or OFL. A name counts only as a word of its own, so `permit licensees` names
/// no MIT license. A CC-BY or CC-BY-SA in text that also names NonCommercial or NoDerivatives is
/// another license. `Unknown` when it names none of them.
pub fn license_kind(text: &str) -> LicenseKind {
    let text = squash(text);
    let kind = LICENSE_NAMES
        .iter()
        .filter_map(|(name, classify)| {
            text.match_indices(name)
                .filter(|(at, _)| !text[..*at].ends_with(|c: char| c.is_alphanumeric()))
                .find_map(|(at, _)| Some((at, classify(&text[at + name.len()..])?)))
        })
        .min_by_key(|(at, _)| *at)
        .map_or(LicenseKind::Unknown, |(_, kind)| kind);
    let restricted = || {
        let text = compact(&text);
        CC_RESTRICTIONS.iter().any(|r| text.contains(r))
    };
    match kind {
        LicenseKind::CcBy | LicenseKind::CcBySa if restricted() => LicenseKind::Unknown,
        kind => kind,
    }
}

/// A format's place in `MAIN_FORMATS`, ignoring case: lower is better, and every format not
/// listed ranks last.
pub fn format_rank(ext: &str) -> usize {
    let ext = ext.to_ascii_lowercase();
    MAIN_FORMATS
        .iter()
        .position(|f| *f == ext)
        .unwrap_or(MAIN_FORMATS.len())
}

/// The format a folder named `name` holds copies in, if it is a format folder.
fn format_folder(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    if name == "fbx(unity)" || name == "fbx (unity)" {
        return Some("fbx");
    }
    let base = name
        .strip_suffix(" format")
        .or_else(|| name.strip_suffix(" files"))
        .unwrap_or(&name);
    FORMAT_FOLDERS.iter().copied().find(|f| *f == base)
}

/// `dir` without its format folders, and the best `format_rank` among them, `None` when it has
/// none.
fn strip_format_folders(dir: &str) -> (String, Option<usize>) {
    let mut best = None;
    let kept: Vec<&str> = dir
        .split('/')
        .filter(|name| match format_folder(name) {
            Some(format) => {
                let rank = format_rank(format);
                best = Some(best.map_or(rank, |b: usize| b.min(rank)));
                false
            }
            None => true,
        })
        .collect();
    (kept.join("/"), best)
}

/// The format groups of `paths` (plan C11), as `Structure::groups` holds them:
///
/// - **Models** group when their lowercase stems are equal, and so are their folders once
///   format folders are taken out (`Models/GLB format/barrel.glb`, `Models/FBX format/barrel.fbx`),
///   which includes models side by side in one folder. The main item has the best
///   `format_rank`.
/// - **Other files under format folders** group the same way when their extensions are equal
///   too, such as a kit's `colormap.png` copies. The main item is the copy under the best format
///   folder.
///
/// Ties break by path, ignoring ASCII case as SQLite's `NOCASE` does. Support files never group.
fn format_groups(
    paths: &[&str],
    kinds: &[FileKind],
    roles: &[Option<SupportRole>],
) -> Vec<Option<usize>> {
    // (folder without format folders, stem, extension of a copy that isn't a model)
    type Key<'k> = (String, &'k str, Option<&'k str>);
    let mut candidates: HashMap<Key, Vec<(usize, usize)>> = HashMap::new();
    for (i, (path, kind)) in paths.iter().zip(kinds).enumerate() {
        if roles[i].is_some() {
            continue;
        }
        let (dir, best_folder) = strip_format_folders(split_dir(path).0);
        let (key, rank) = if kind.is_model {
            ((dir, kind.stem.as_str(), None), format_rank(&kind.ext))
        } else if let Some(rank) = best_folder {
            ((dir, kind.stem.as_str(), Some(kind.ext.as_str())), rank)
        } else {
            continue;
        };
        candidates.entry(key).or_default().push((rank, i));
    }

    let mut groups = vec![None; paths.len()];
    form_groups(paths, candidates.into_values(), &mut groups);
    groups
}

/// Records in `groups` each of `candidates` that holds more than one file, as (rank, input
/// index) pairs: the file with the lowest rank leads it, ties broken by path, ignoring ASCII case
/// as SQLite's `NOCASE` does.
fn form_groups<R: Ord>(
    paths: &[&str],
    candidates: impl IntoIterator<Item = Vec<(R, usize)>>,
    groups: &mut [Option<usize>],
) {
    for files in candidates.into_iter().filter(|f| f.len() > 1) {
        let (_, main) = files
            .iter()
            .min_by(|(rank_a, a), (rank_b, b)| {
                let (a, b) = (paths[*a], paths[*b]);
                rank_a
                    .cmp(rank_b)
                    .then_with(|| {
                        a.bytes()
                            .map(|c| c.to_ascii_lowercase())
                            .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
                    })
                    .then_with(|| a.cmp(b))
            })
            .expect("a group has files");
        let main = *main;
        for (_, i) in files {
            groups[i] = Some(main);
        }
    }
}

/// What a folder named `name` says about the sprite copies inside it (plan C13).
#[derive(Debug, PartialEq, Eq)]
enum StackFolder {
    /// A scale or format folder, which a stack's folder leaves out: `Default`, `Double (128px)`,
    /// `Vector`. `larger` is true for a scale above the default.
    Copies { larger: bool },
    /// A folder named after another with a larger scale's ending, which a stack's folder names
    /// as that other: `Sprites X2`, `Poses HD`, `Dark (2×)`.
    LargerOf(String),
}

/// The folder `name` is in a 2D stack's terms, if it is a scale or format folder, ignoring case
/// and a parenthesized note (`Default (64px)`).
fn stack_folder(name: &str) -> Option<StackFolder> {
    let lower = name.to_lowercase();
    let base = match lower.rsplit_once(" (") {
        Some((base, note)) if note.ends_with(')') => base,
        _ => lower.as_str(),
    };
    if DEFAULT_SCALE_FOLDERS.contains(&base) || IMAGE_FORMAT_FOLDERS.contains(&base) {
        return Some(StackFolder::Copies { larger: false });
    }
    if LARGER_SCALE_FOLDERS.contains(&base) {
        return Some(StackFolder::Copies { larger: true });
    }
    LARGER_SCALE_SUFFIXES
        .iter()
        .find(|suffix| lower.ends_with(*suffix) && lower.len() > suffix.len())
        .map(|suffix| StackFolder::LargerOf(lower[..lower.len() - suffix.len()].to_string()))
}

/// `dir` as a 2D stack's folder (lowercase, without its scale and format folders, each folder
/// with a larger scale's ending named as the folder it scales), and whether it holds a larger
/// scale than the default.
fn strip_stack_folders(dir: &str) -> (String, bool) {
    let mut larger = false;
    let kept: Vec<String> = dir
        .split('/')
        .filter_map(|name| match stack_folder(name) {
            Some(StackFolder::Copies { larger: l }) => {
                larger |= l;
                None
            }
            Some(StackFolder::LargerOf(base)) => {
                larger = true;
                Some(base)
            }
            None => Some(name.to_lowercase()),
        })
        .collect();
    (kept.join("/"), larger)
}

/// Adds the 2D stacks of `paths` (plan C13) to `groups`: images (`texture` and `image` kinds)
/// that are in no group and are no support file stack when their lowercase stems are equal, and
/// so are their folders once scale and format folders are taken out (`PNG/Blue/Default/`,
/// `PNG/Blue/Double/` and `Vector/Blue/` are all `Blue/`). Style and color folders (`Dark`,
/// `Blue`) are none of these, so their sprites stay apart. The main item is a PNG before other
/// formats, then a copy at the default scale (in no scale folder, or in `Default`, `Default size`
/// or `1x`) before larger ones, then the path that sorts first ignoring case.
fn stacks(
    paths: &[&str],
    kinds: &[FileKind],
    roles: &[Option<SupportRole>],
    groups: &mut [Option<usize>],
) {
    // (folder without scale and format folders, stem)
    type Key<'k> = (String, &'k str);
    // (not a PNG, at a larger scale)
    type Rank = (bool, bool);
    let mut candidates: HashMap<Key, Vec<(Rank, usize)>> = HashMap::new();
    for (i, (path, kind)) in paths.iter().zip(kinds).enumerate() {
        if !kind.is_image || roles[i].is_some() || groups[i].is_some() {
            continue;
        }
        let (dir, larger) = strip_stack_folders(split_dir(path).0);
        candidates
            .entry((dir, kind.stem.as_str()))
            .or_default()
            .push(((kind.ext != "png", larger), i));
    }
    form_groups(paths, candidates.into_values(), groups);
}

/// Whether `dir` is a pack or lies inside one.
fn in_pack(packs: &BTreeSet<&str>, dir: &str) -> bool {
    packs.contains(dir) || ancestors(dir).any(|a| packs.contains(a))
}

/// Whether a pack lies below `dir`. Every path below `dir` starts with `dir/`, and those paths
/// sort together from `dir/` on.
fn holds_pack(packs: &BTreeSet<&str>, dir: &str) -> bool {
    let below = format!("{dir}/");
    packs
        .range::<str, _>((Bound::Included(below.as_str()), Bound::Unbounded))
        .next()
        .is_some_and(|p| p.starts_with(&below))
}

fn folder(parent_path: Option<&str>, name: &str) -> Folder {
    Folder {
        parent_path: parent_path.map(str::to_string),
        name: name.to_string(),
        is_pack: false,
        item_count: 0,
        hidden_count: 0,
        split_groups: 0,
        pack: None,
    }
}

/// One indexed file as the pass reads it.
#[derive(sqlx::FromRow)]
struct FileRow {
    id: String,
    file_path: String,
    extension: String,
    support_role: Option<String>,
    group_id: Option<String>,
    bundled_preview: Option<String>,
    preview_status: String,
    size_bytes: i64,
    modified_at: i64,
}

pub(crate) type FolderRow = (
    String,
    Option<String>,
    String,
    bool,
    i64,
    i64,
    i64,
    Option<String>,
);

/// A `pack_json` value as `PackInfo`; one that doesn't parse is rewritten by the pass.
pub(crate) fn parse_pack(json: Option<&str>) -> Option<PackInfo> {
    serde_json::from_str(json?).ok()
}

/// The license files the pass read before, by path, from the current `pack_json`s.
async fn known_licenses(pool: &Pool<Sqlite>) -> sqlx::Result<HashMap<String, LicenseFile>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT pack_json FROM folders WHERE pack_json IS NOT NULL")
            .fetch_all(pool)
            .await?;
    Ok(rows
        .iter()
        .filter_map(|(json,)| parse_pack(Some(json))?.license)
        .map(|l| (l.path.clone(), l))
        .collect())
}

/// What license file `file` said when the pass last read it, when its size and mtime haven't
/// changed since and the same `LICENSE_PARSER` read it; `None` when it must be read again.
fn known_text(file: &FileRow, known: &HashMap<String, LicenseFile>) -> Option<LicenseText> {
    known
        .get(&file.file_path)
        .filter(|l| {
            (l.parser, l.size_bytes, l.modified_at)
                == (LICENSE_PARSER, file.size_bytes, file.modified_at)
        })
        .map(|l| l.text.clone())
}

/// What each license file of `paths` below `root` says, from its first `HEADER_BYTES`, or why
/// it couldn't be read. The reads run on the blocking pool, off the async runtime, since a
/// library on a network share can take long to open each file.
async fn read_license_files(
    root: &Path,
    paths: Vec<String>,
) -> HashMap<String, std::io::Result<LicenseText>> {
    use std::io::Read;
    let read_all = {
        let (root, paths) = (root.to_path_buf(), paths.clone());
        move || {
            paths
                .into_iter()
                .map(|path| {
                    let mut head = Vec::new();
                    let text = std::fs::File::open(root.join(&path))
                        .and_then(|f| f.take(HEADER_BYTES).read_to_end(&mut head))
                        .map(|_| parse_license(&String::from_utf8_lossy(&head)));
                    (path, text)
                })
                .collect()
        }
    };
    tokio::task::spawn_blocking(read_all)
        .await
        .unwrap_or_else(|e| {
            let error = format!("the license reads stopped: {e}");
            paths
                .into_iter()
                .map(|path| (path, Err(std::io::Error::other(error.clone()))))
                .collect()
        })
}

/// Makes `folders` hold exactly `folders`, writing only the rows that differ, in transactions of
/// `BATCH` rows. Returns how many rows it inserted, updated or deleted.
async fn apply_folders(
    pool: &Pool<Sqlite>,
    folders: &BTreeMap<String, Folder>,
) -> sqlx::Result<usize> {
    let rows: Vec<FolderRow> = sqlx::query_as(
        "SELECT path, parent_path, name, is_pack, item_count, hidden_count, split_groups, pack_json
         FROM folders",
    )
    .fetch_all(pool)
    .await?;
    let current: HashMap<String, Folder> = rows
        .into_iter()
        .map(
            |(
                path,
                parent_path,
                name,
                is_pack,
                item_count,
                hidden_count,
                split_groups,
                pack_json,
            )| {
                let folder = Folder {
                    parent_path,
                    name,
                    is_pack,
                    item_count,
                    hidden_count,
                    split_groups,
                    pack: parse_pack(pack_json.as_deref()),
                };
                (path, folder)
            },
        )
        .collect();
    let changed: Vec<(&String, &Folder)> = folders
        .iter()
        .filter(|(path, folder)| current.get(*path) != Some(folder))
        .collect();
    let gone: Vec<&String> = current
        .keys()
        .filter(|path| !folders.contains_key(*path))
        .collect();

    for chunk in changed.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for (path, folder) in chunk {
            let pack_json = folder
                .pack
                .as_ref()
                .map(|p| serde_json::to_string(p).expect("pack info serializes"));
            sqlx::query(
                "INSERT INTO folders (path, parent_path, name, is_pack, item_count, hidden_count,
                     split_groups, pack_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(path) DO UPDATE SET parent_path = excluded.parent_path,
                     name = excluded.name, is_pack = excluded.is_pack,
                     item_count = excluded.item_count, hidden_count = excluded.hidden_count,
                     split_groups = excluded.split_groups, pack_json = excluded.pack_json",
            )
            .bind(path)
            .bind(&folder.parent_path)
            .bind(&folder.name)
            .bind(folder.is_pack)
            .bind(folder.item_count)
            .bind(folder.hidden_count)
            .bind(folder.split_groups)
            .bind(pack_json)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }
    for chunk in gone.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for path in chunk {
            sqlx::query("DELETE FROM folders WHERE path = ?")
                .bind(path)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
    Ok(changed.len() + gone.len())
}

/// Sets `support_role` on each of `files` whose role differs from `roles` (in the same order),
/// in transactions of `BATCH` rows. Returns how many rows it updated.
async fn apply_roles(
    pool: &Pool<Sqlite>,
    files: &[FileRow],
    roles: &[Option<SupportRole>],
) -> sqlx::Result<usize> {
    let changed: Vec<(&str, Option<&str>)> = files
        .iter()
        .zip(roles)
        .map(|(file, role)| (file, role.map(SupportRole::as_str)))
        .filter(|(file, role)| file.support_role.as_deref() != *role)
        .map(|(file, role)| (file.id.as_str(), role))
        .collect();
    for chunk in changed.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for (id, role) in chunk {
            sqlx::query("UPDATE assets SET support_role = ? WHERE id = ?")
                .bind(role)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
    Ok(changed.len())
}

/// Sets `group_id` on each of `files` whose main item differs from `groups` (in the same order),
/// in transactions of `BATCH` rows. Returns how many rows it updated.
async fn apply_groups(
    pool: &Pool<Sqlite>,
    files: &[FileRow],
    groups: &[Option<usize>],
) -> sqlx::Result<usize> {
    let changed: Vec<(&str, Option<&str>)> = files
        .iter()
        .zip(groups)
        .map(|(file, main)| (file, main.map(|m| files[m].id.as_str())))
        .filter(|(file, main)| file.group_id.as_deref() != *main)
        .map(|(file, main)| (file.id.as_str(), main))
        .collect();
    for chunk in changed.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for (id, main) in chunk {
            sqlx::query("UPDATE assets SET group_id = ? WHERE id = ?")
                .bind(main)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
    Ok(changed.len())
}

/// The files a metadata write on `id` reaches (plan C20): every member of its group, the main
/// item included, or `id` alone for a file on its own. Support files never group, so a model's
/// bundled preview is never one. Rating, color label, favorite, description, tag and collection
/// writes all go through it, in their own transaction.
pub async fn group_members<'e, E>(executor: E, id: &str) -> sqlx::Result<Vec<String>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    sqlx::query_scalar(
        "SELECT ?1 UNION SELECT id FROM assets WHERE group_id = (SELECT group_id FROM assets WHERE id = ?1)",
    )
    .bind(id)
    .fetch_all(executor)
    .await
}

/// The `library_meta` key recording that every group of a library had its metadata unioned
/// once. Groups formed before group writes existed (plan step 3) never were.
const GROUP_UNION_KEY: &str = "group_metadata_union";

/// One file's metadata, as a group's union reads it. Each field's default means unset.
#[derive(sqlx::FromRow)]
struct Metadata {
    rating: i64,
    color_label: String,
    is_favorite: bool,
    description: String,
}

/// Unions the metadata of every group that forms or grows (plan C14), or of every group once per
/// library (`GROUP_UNION_KEY`), in transactions of `BATCH` groups. Each member gets every tag and
/// collection a member has, and the main item's rating, color label, favorite and description.
/// A field the main item leaves unset takes the first member's that is set, by path, so a GLB
/// that joins a rated FBX doesn't drop its rating. It runs before `apply_groups` and reads the
/// groups from `groups`, so a pass interrupted between the two unions again. A dissolved group's
/// files keep what they have. Returns how many groups it unioned.
async fn apply_unions(
    pool: &Pool<Sqlite>,
    files: &[FileRow],
    groups: &[Option<usize>],
) -> sqlx::Result<usize> {
    let all: bool =
        sqlx::query_scalar("SELECT NOT EXISTS (SELECT 1 FROM library_meta WHERE key = ?)")
            .bind(GROUP_UNION_KEY)
            .fetch_one(pool)
            .await?;
    let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (i, main) in groups.iter().enumerate() {
        if let Some(main) = *main {
            members.entry(main).or_default().push(i);
        }
    }
    let changed: Vec<(usize, Vec<usize>)> = members
        .into_iter()
        .filter(|(main, group)| {
            all || group
                .iter()
                .any(|&i| files[i].group_id.as_deref() != Some(files[*main].id.as_str()))
        })
        .collect();
    for chunk in changed.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for (main, group) in chunk {
            let mut group = group.clone();
            group.sort_by_key(|&i| (i != *main, files[i].file_path.to_ascii_lowercase()));
            let ids: Vec<&str> = group.iter().map(|&i| files[i].id.as_str()).collect();
            let ids = serde_json::to_string(&ids).expect("ids serialize");
            sqlx::query(
                "INSERT OR IGNORE INTO asset_tags (asset_id, tag_id)
                 SELECT m.value, t.tag_id FROM json_each(?1) m
                 JOIN asset_tags t ON t.asset_id IN (SELECT value FROM json_each(?1))",
            )
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT OR IGNORE INTO collection_assets (collection_id, asset_id)
                 SELECT c.collection_id, m.value FROM json_each(?1) m
                 JOIN collection_assets c ON c.asset_id IN (SELECT value FROM json_each(?1))",
            )
            .bind(&ids)
            .execute(&mut *tx)
            .await?;

            let rows: Vec<Metadata> = sqlx::query_as(
                "SELECT a.rating, a.color_label, a.is_favorite, a.description
                 FROM json_each(?) m JOIN assets a ON a.id = m.value ORDER BY m.key",
            )
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await?;
            let first = |set: fn(&Metadata) -> bool| rows.iter().find(|r| set(r));
            let rating = first(|r| r.rating != 0).map_or(0, |r| r.rating);
            let color_label = first(|r| r.color_label != "none" && !r.color_label.is_empty())
                .map_or("none", |r| r.color_label.as_str());
            let is_favorite = rows.iter().any(|r| r.is_favorite);
            let description =
                first(|r| !r.description.is_empty()).map_or("", |r| r.description.as_str());
            sqlx::query(
                "UPDATE assets SET rating = ?, color_label = ?, is_favorite = ?, description = ?
                 WHERE id IN (SELECT value FROM json_each(?))",
            )
            .bind(rating)
            .bind(color_label)
            .bind(is_favorite)
            .bind(description)
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }
    if all {
        sqlx::query(
            "INSERT INTO library_meta (key, value) VALUES (?, 'done')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(GROUP_UNION_KEY)
        .execute(pool)
        .await?;
    }
    Ok(changed.len())
}

/// Sets `bundled_preview` on each of `files` whose render differs from `bundled` (in the same
/// order), to the render's path, in transactions of `BATCH` rows. Returns how many rows it
/// updated.
async fn apply_bundled_previews(
    pool: &Pool<Sqlite>,
    files: &[FileRow],
    bundled: &[Option<usize>],
) -> sqlx::Result<usize> {
    let changed: Vec<(&str, Option<&str>)> = files
        .iter()
        .zip(bundled)
        .map(|(file, render)| (file, render.map(|r| files[r].file_path.as_str())))
        .filter(|(file, render)| file.bundled_preview.as_deref() != *render)
        .map(|(file, render)| (file.id.as_str(), render))
        .collect();
    for chunk in changed.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for (id, render) in chunk {
            sqlx::query("UPDATE assets SET bundled_preview = ? WHERE id = ?")
                .bind(render)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
    Ok(changed.len())
}

/// Keeps group members and models with a bundled preview off the preview queue (plan C15). A
/// group member other than its main item comes off the queue, keeping any thumbnail it has,
/// unless it is its group's stand-in (`preview_stand_ins`), and so does an item whose card shows
/// its bundled preview. Any other file at `none` whose format takes a preview goes on it: a new
/// file, a member that became a main item with no picture, a stand-in, or a pack's cover image or
/// a render, which "Show hidden files" shows and pack and model cards picture.
/// `preview::unqueue` and `preview::enqueue` own how. Returns how many assets it changed.
async fn apply_previews(
    pool: &Pool<Sqlite>,
    files: &[FileRow],
    structure: &Structure,
) -> sqlx::Result<usize> {
    let waiting = crate::preview::assets_waiting_for_jobs(pool).await?;
    let stand_ins = preview_stand_ins(files, structure);
    let mut members = Vec::new();
    let mut items = Vec::new();
    for (i, file) in files.iter().enumerate() {
        let main = structure.groups[i];
        let pictured = structure.bundled_previews[i].is_some();
        if (main.is_some_and(|m| m != i) && !stand_ins.contains(&i)) || pictured {
            if file.preview_status == "pending" || waiting.contains(&file.id) {
                members.push(file.id.as_str());
            }
        } else if file.preview_status == "none" && crate::preview::needs_preview(&file.extension) {
            items.push(file.id.as_str());
        }
    }
    for chunk in members.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for id in chunk {
            crate::preview::unqueue(&mut tx, id).await?;
        }
        tx.commit().await?;
    }
    let now = chrono::Utc::now().timestamp_millis();
    for chunk in items.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for id in chunk {
            crate::preview::enqueue(&mut tx, id, now).await?;
        }
        tx.commit().await?;
    }
    Ok(members.len() + items.len())
}

/// The member that is previewed for its group, by index into `files`, for each group whose main
/// item's format takes no preview, such as a `.blend` beside an `.obj`, and that has no bundled
/// preview: the previewable member that ranks first by format, then by path ignoring case. The
/// card shows its thumbnail (`commands::asset`'s `ASSET_COLUMNS`), and `preview::claim_jobs`
/// lets its job through.
fn preview_stand_ins(files: &[FileRow], structure: &Structure) -> HashSet<usize> {
    let mut best: HashMap<usize, usize> = HashMap::new();
    let key = |i: usize| {
        let file = &files[i];
        (
            format_rank(&file.extension),
            file.file_path.to_ascii_lowercase(),
            file.file_path.clone(),
        )
    };
    for (i, main) in structure.groups.iter().enumerate() {
        let Some(main) = *main else { continue };
        if main == i
            || structure.bundled_previews[main].is_some()
            || crate::preview::needs_preview(&files[main].extension)
            || !crate::preview::needs_preview(&files[i].extension)
        {
            continue;
        }
        best.entry(main)
            .and_modify(|b| {
                if key(i) < key(*b) {
                    *b = i;
                }
            })
            .or_insert(i);
    }
    best.into_values().collect()
}

/// Runs the pass on the library DB at `pool`, whose files lie below `root`: reads every indexed
/// path, derives the structure and applies it. It reads a pack's license file only when its size
/// or mtime changed since the last pass. Returns the rows written. The caller holds the library's
/// `index_lock`.
pub async fn rebuild(pool: &Pool<Sqlite>, root: &Path) -> sqlx::Result<usize> {
    let started = Instant::now();
    let files: Vec<FileRow> = sqlx::query_as(
        "SELECT id, file_path, extension, support_role, group_id, bundled_preview, preview_status,
                size_bytes, modified_at
         FROM assets",
    )
    .fetch_all(pool)
    .await?;
    let known = known_licenses(pool).await?;
    let by_path: HashMap<&str, &FileRow> =
        files.iter().map(|f| (f.file_path.as_str(), f)).collect();
    let inputs = || files.iter().map(|f| (f.file_path.as_str(), f.size_bytes));
    // Derive with the licenses already read, noting each one that changed since. Only then read
    // those, off the runtime, and derive again with what they say.
    let mut changed = Vec::new();
    let mut structure = derive(inputs(), |path| {
        known_text(by_path[path], &known).unwrap_or_else(|| {
            changed.push(path.to_string());
            LicenseText::default()
        })
    });
    let mut unread = HashSet::new();
    let reads = changed.len();
    if !changed.is_empty() {
        let read = read_license_files(root, changed).await;
        structure = derive(inputs(), |path| match read.get(path) {
            None => known_text(by_path[path], &known).unwrap_or_default(),
            Some(Ok(text)) => text.clone(),
            // An unreadable license keeps what it said last time, and records no read, so the
            // next pass tries it again.
            Some(Err(e)) => {
                tracing::debug!(target: "structure", "Could not read {path}: {e}");
                unread.insert(path.to_string());
                known.get(path).map(|l| l.text.clone()).unwrap_or_default()
            }
        });
    }
    // A read license records the parser, size and mtime it was read with; an unread one records
    // none.
    for license in structure
        .folders
        .values_mut()
        .filter_map(|f| f.pack.as_mut()?.license.as_mut())
        .filter(|l| !unread.contains(&l.path))
    {
        let file = by_path[license.path.as_str()];
        (license.parser, license.size_bytes, license.modified_at) =
            (LICENSE_PARSER, file.size_bytes, file.modified_at);
    }
    let structure = structure;

    let mut written = apply_folders(pool, &structure.folders).await?
        + apply_roles(pool, &files, &structure.roles).await?;
    // A union follows from the `group_id` writes counted next, so it isn't counted itself.
    let unioned = apply_unions(pool, &files, &structure.groups).await?;
    written += apply_groups(pool, &files, &structure.groups).await?
        + apply_bundled_previews(pool, &files, &structure.bundled_previews).await?
        + apply_previews(pool, &files, &structure).await?;
    let grouped = structure.groups.iter().flatten().count();
    let mains = structure
        .groups
        .iter()
        .enumerate()
        .filter(|(i, main)| **main == Some(*i))
        .count();
    tracing::info!(
        target: "structure",
        "Structure pass: {} files, {} folders, {} packs, {} license files read, {} support files, {} groups of {} files, {} groups unioned, {} bundled previews, {} rows written in {:?}",
        files.len(),
        structure.folders.len(),
        structure.folders.values().filter(|f| f.is_pack).count(),
        reads,
        structure.roles.iter().flatten().count(),
        mains,
        grouped,
        unioned,
        structure.bundled_previews.iter().flatten().count(),
        written,
        started.elapsed()
    );
    Ok(written)
}

/// Reruns the pass for `library` once any index run on it has finished, for a command that
/// changed its paths. A failed pass is logged, not returned: the change itself went through,
/// and the next index run derives the structure again.
pub async fn rebuild_library(db: &Database, library: &LibraryHandle) {
    let lock = db.index_lock(&library.id);
    let _guard = lock.lock().await;
    if let Err(e) = rebuild(&library.pool, &library.root_path).await {
        tracing::error!(target: "structure", "Structure pass failed for library {}: {e}", library.id);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::memory_library_pool;

    /// A library root that doesn't exist, for passes that read no license file.
    pub(crate) fn unmounted() -> &'static Path {
        Path::new("/opendam-test-library-not-mounted")
    }

    /// The structure of `paths` when every file is 1 byte and no license file says anything.
    fn derive<'a>(paths: impl IntoIterator<Item = &'a str>) -> Structure {
        super::derive(paths.into_iter().map(|p| (p, 1)), |_| {
            LicenseText::default()
        })
    }

    fn folder(parent_path: Option<&str>, name: &str, item_count: i64, hidden_count: i64) -> Folder {
        Folder {
            parent_path: parent_path.map(str::to_string),
            name: name.to_string(),
            is_pack: false,
            item_count,
            hidden_count,
            split_groups: 0,
            pack: None,
        }
    }

    fn split(folder: Folder, split_groups: i64) -> Folder {
        Folder {
            split_groups,
            ..folder
        }
    }

    /// `folder` as a pack named after its folder, with `cover`, `license` and the files of
    /// `kinds`, 1 byte each, and no gallery.
    fn pack(
        folder: Folder,
        cover: Option<&str>,
        license: Option<&str>,
        kinds: &[(&str, i64)],
    ) -> Folder {
        Folder {
            is_pack: true,
            pack: Some(PackInfo {
                name: folder.name.clone(),
                cover: cover.map(str::to_string),
                gallery: Vec::new(),
                license: license.map(|path| LicenseFile {
                    path: path.to_string(),
                    text: LicenseText::default(),
                    parser: 0,
                    size_bytes: 0,
                    modified_at: 0,
                }),
                kinds: kinds.iter().map(|(k, n)| (k.to_string(), *n)).collect(),
                size_bytes: kinds.iter().map(|(_, n)| n).sum(),
            }),
            ..folder
        }
    }

    /// The pack info of `path` in `structure`.
    fn info<'s>(structure: &'s Structure, path: &str) -> &'s PackInfo {
        structure.folders[path].pack.as_ref().expect("a pack")
    }

    /// The packs of `structure`, by path.
    fn packs(structure: &Structure) -> Vec<&str> {
        structure
            .folders
            .iter()
            .filter(|(_, f)| f.is_pack)
            .map(|(path, _)| path.as_str())
            .collect()
    }

    /// Each support file of `paths` with its role, in input order.
    fn hidden<'a>(paths: &[&'a str]) -> Vec<(&'a str, &'static str)> {
        let structure = derive(paths.iter().copied());
        paths
            .iter()
            .zip(&structure.roles)
            .filter_map(|(path, role)| role.map(|r| (*path, r.as_str())))
            .collect()
    }

    /// A Kenney-like layout: two kits in one category, one sharing the other's name as a prefix
    /// and one shipping a model in two formats, a file at the root and a file directly in a
    /// category.
    const KENNEY: [&str; 7] = [
        "3D assets/Platformer Kit/License.txt",
        "3D assets/Platformer Kit/Models/GLB format/block.glb",
        "3D assets/Platformer Kit/Models/OBJ format/block.obj",
        "3D assets/Platformer Kit 2/crate.glb",
        "3D assets/index.md",
        "2D assets/UI Pack/PNG/button.png",
        "Readme.txt",
    ];

    #[test]
    fn every_folder_counts_the_visible_and_the_support_files_below_it() {
        let structure = derive(KENNEY);
        // `block.glb` and `block.obj` are one item, counted once in each folder above them.
        let expected = BTreeMap::from(
            [
                ("", folder(None, "", 5, 1)),
                ("2D assets", folder(Some(""), "2D assets", 1, 0)),
                (
                    "2D assets/UI Pack",
                    folder(Some("2D assets"), "UI Pack", 1, 0),
                ),
                (
                    "2D assets/UI Pack/PNG",
                    folder(Some("2D assets/UI Pack"), "PNG", 1, 0),
                ),
                ("3D assets", folder(Some(""), "3D assets", 3, 1)),
                (
                    "3D assets/Platformer Kit",
                    pack(
                        folder(Some("3D assets"), "Platformer Kit", 1, 1),
                        Some("3D assets/Platformer Kit/Models/GLB format/block.glb"),
                        Some("3D assets/Platformer Kit/License.txt"),
                        &[("3d_model", 2), ("document", 1)],
                    ),
                ),
                (
                    "3D assets/Platformer Kit 2",
                    folder(Some("3D assets"), "Platformer Kit 2", 1, 0),
                ),
                (
                    "3D assets/Platformer Kit/Models",
                    folder(Some("3D assets/Platformer Kit"), "Models", 1, 0),
                ),
                (
                    "3D assets/Platformer Kit/Models/GLB format",
                    folder(Some("3D assets/Platformer Kit/Models"), "GLB format", 1, 0),
                ),
                (
                    "3D assets/Platformer Kit/Models/OBJ format",
                    split(
                        folder(Some("3D assets/Platformer Kit/Models"), "OBJ format", 1, 0),
                        1,
                    ),
                ),
            ]
            .map(|(path, f)| (path.to_string(), f)),
        );
        assert_eq!(structure.folders, expected);
        assert_eq!(
            hidden(&KENNEY),
            [("3D assets/Platformer Kit/License.txt", "doc")]
        );
    }

    #[test]
    fn a_library_with_no_files_has_only_its_root() {
        let structure = derive([]);
        assert_eq!(
            structure.folders,
            BTreeMap::from([(String::new(), folder(None, "", 0, 0))])
        );
        assert!(structure.roles.is_empty());
    }

    #[test]
    fn files_only_at_the_root_make_no_folders() {
        assert_eq!(
            derive(["a.png", "b.glb"]).folders,
            BTreeMap::from([(String::new(), folder(None, "", 2, 0))])
        );
    }

    #[test]
    fn the_order_of_the_paths_does_not_matter() {
        let mut reversed = KENNEY;
        reversed.reverse();
        let mut roles = derive(KENNEY).roles;
        roles.reverse();
        let last = KENNEY.len() - 1;
        let mut groups: Vec<Option<usize>> = derive(KENNEY)
            .groups
            .into_iter()
            .map(|main| main.map(|m| last - m))
            .collect();
        groups.reverse();
        let mut bundled_previews: Vec<Option<usize>> = derive(KENNEY)
            .bundled_previews
            .into_iter()
            .map(|render| render.map(|r| last - r))
            .collect();
        bundled_previews.reverse();
        assert_eq!(
            derive(reversed),
            Structure {
                roles,
                groups,
                bundled_previews,
                ..derive(KENNEY)
            }
        );
    }

    /// A Kenney 3D kit: markers at its root, models in format folders, renders in `Previews/`.
    const KENNEY_3D_KIT: [&str; 9] = [
        "3D assets/Car Kit/License.txt",
        "3D assets/Car Kit/Preview.png",
        "3D assets/Car Kit/Sample.png",
        "3D assets/Car Kit/Models/GLB format/sedan.glb",
        "3D assets/Car Kit/Models/GLB format/Textures/colormap.png",
        "3D assets/Car Kit/Models/FBX format/sedan.fbx",
        "3D assets/Car Kit/Models/OBJ format/sedan.obj",
        "3D assets/Car Kit/Previews/sedan.png",
        "3D assets/Holiday Kit/License.txt",
    ];

    #[test]
    fn a_kenney_3d_kit_is_a_pack_whose_docs_covers_and_renders_are_hidden() {
        let structure = derive(KENNEY_3D_KIT);
        assert_eq!(
            packs(&structure),
            ["3D assets/Car Kit", "3D assets/Holiday Kit"]
        );
        assert_eq!(
            hidden(&KENNEY_3D_KIT),
            [
                ("3D assets/Car Kit/License.txt", "doc"),
                ("3D assets/Car Kit/Preview.png", "cover"),
                ("3D assets/Car Kit/Sample.png", "cover"),
                ("3D assets/Car Kit/Previews/sedan.png", "model_preview"),
                ("3D assets/Holiday Kit/License.txt", "doc"),
            ]
        );
        // The sedan in three formats is one item, next to the texture.
        let kit = &structure.folders["3D assets/Car Kit"];
        assert_eq!((kit.item_count, kit.hidden_count), (2, 4));
        // A pack whose only file is hidden keeps its row, with no visible item.
        let holiday = &structure.folders["3D assets/Holiday Kit"];
        assert_eq!((holiday.item_count, holiday.hidden_count), (0, 1));
    }

    /// Kenney 2D in the current style: several samples, a tilemap and tiles, and a Readme in a
    /// Construct project folder.
    const KENNEY_2D: [&str; 8] = [
        "2D assets/Pixel Platformer/License.txt",
        "2D assets/Pixel Platformer/Preview.png",
        "2D assets/Pixel Platformer/SampleA.png",
        "2D assets/Pixel Platformer/SampleB.png",
        "2D assets/Pixel Platformer/Tiles/tile_0000.png",
        "2D assets/Pixel Platformer/Tiles/Characters/tile_0000.png",
        "2D assets/Pixel Platformer/Tilemap/tilemap.png",
        "2D assets/Pixel Platformer/Construct 3/Readme.txt",
    ];

    /// Kenney 2D in the old style: PNG, spritesheet and vector folders.
    const KENNEY_2D_OLD: [&str; 6] = [
        "2D assets/Abstract Platformer/License.txt",
        "2D assets/Abstract Platformer/Preview.png",
        "2D assets/Abstract Platformer/Sample.png",
        "2D assets/Abstract Platformer/PNG/Players/Player Blue/playerBlue_walk1.png",
        "2D assets/Abstract Platformer/Spritesheet/spritesheet_complete.png",
        "2D assets/Abstract Platformer/Vector/vector_complete.svg",
    ];

    /// The UI pack: color folders, each with scale folders, and vectors.
    const UI_PACK: [&str; 7] = [
        "UI assets/UI Pack/License.txt",
        "UI assets/UI Pack/Preview.png",
        "UI assets/UI Pack/Sample.png",
        "UI assets/UI Pack/PNG/Blue/Default/button_rectangle.png",
        "UI assets/UI Pack/PNG/Blue/Double/button_rectangle.png",
        "UI assets/UI Pack/Vector/Blue/button_rectangle.svg",
        "UI assets/UI Pack/Spritesheet/blueSheet.png",
    ];

    #[test]
    fn kenney_2d_packs_in_both_styles_and_the_ui_pack_are_packs_with_hidden_covers() {
        for fixture in [&KENNEY_2D[..], &KENNEY_2D_OLD, &UI_PACK] {
            let pack_root = fixture[0].strip_suffix("/License.txt").unwrap();
            assert_eq!(packs(&derive(fixture.iter().copied())), [pack_root]);
            let expected: Vec<(&str, &str)> = fixture
                .iter()
                .filter(|p| split_dir(p).0 == pack_root)
                .map(|p| (*p, if p.ends_with(".txt") { "doc" } else { "cover" }))
                .collect();
            assert_eq!(hidden(fixture), expected, "{pack_root}");
        }
        // A Readme outside the pack's own folder and its marked folders stays visible.
        assert!(!hidden(&KENNEY_2D)
            .iter()
            .any(|(p, _)| p.ends_with("Readme.txt")));
    }

    /// `Archive/` and `Early access/` each hold a Readme above the packs inside them, and
    /// `Archive/Pre-rendered Models` has no marker.
    const CATEGORIES_WITH_A_README: [&str; 11] = [
        "Archive/Readme.txt",
        "Archive/Mini Car Kit/License.txt",
        "Archive/Mini Car Kit/Models/FBX format/carSedan.fbx",
        "Archive/Onscreen Controls/License.txt",
        "Archive/Onscreen Controls/Sprites/button.png",
        "Archive/Pre-rendered Models/Isometric/car_NE.png",
        "Early access/Readme.txt",
        "Early access/Medieval Weapons/License.txt",
        "Early access/Medieval Weapons/Models/GLB format/sword.glb",
        "Early access/Racing Kit/License.txt",
        "Early access/Racing Kit/Previews/racer-red.png",
    ];

    #[test]
    fn a_readme_marks_no_pack_so_a_category_holding_one_keeps_its_packs() {
        let structure = derive(CATEGORIES_WITH_A_README);
        assert_eq!(
            packs(&structure),
            [
                "Archive/Mini Car Kit",
                "Archive/Onscreen Controls",
                "Archive/Pre-rendered Models",
                "Early access/Medieval Weapons",
                "Early access/Racing Kit",
            ]
        );
        // The category Readmes are outside every pack, so they stay visible.
        let hidden = hidden(&CATEGORIES_WITH_A_README);
        assert!(!hidden.iter().any(|(p, _)| p.ends_with("Readme.txt")));
        assert_eq!(hidden.len(), 4);
    }

    /// Holiday Pack 2016: markers at its root, and a Readme in each of two inner folders.
    const HOLIDAY_PACK: [&str; 9] = [
        "2D assets/Holiday Pack 2016/License.txt",
        "2D assets/Holiday Pack 2016/Preview.png",
        "2D assets/Holiday Pack 2016/Sample.png",
        "2D assets/Holiday Pack 2016/PNG/RTS pack/Default size/tree.png",
        "2D assets/Holiday Pack 2016/PNG/RTS pack/Retina/tree.png",
        "2D assets/Holiday Pack 2016/Spritesheet/Readme.txt",
        "2D assets/Holiday Pack 2016/Spritesheet/sheet.png",
        "2D assets/Holiday Pack 2016/Tilesheet/Readme.txt",
        "2D assets/Holiday Pack 2016/Tilesheet/tiles.png",
    ];

    #[test]
    fn inner_readmes_do_not_split_a_pack() {
        assert_eq!(
            packs(&derive(HOLIDAY_PACK)),
            ["2D assets/Holiday Pack 2016"]
        );
        assert_eq!(hidden(&HOLIDAY_PACK).len(), 3);
    }

    #[test]
    fn a_marked_folder_inside_a_pack_is_no_pack_but_hides_its_support_files() {
        let paths = [
            "Other/Construct samples 2/License.txt",
            "Other/Construct samples 2/Sources/Preview.png",
            "Other/Construct samples 2/Sources/fishGame.txt",
            "Other/Construct samples 2/Sources/fishGame.c3p.png",
        ];
        assert_eq!(packs(&derive(paths)), ["Other/Construct samples 2"]);
        assert_eq!(
            hidden(&paths),
            [
                ("Other/Construct samples 2/License.txt", "doc"),
                ("Other/Construct samples 2/Sources/Preview.png", "cover"),
                ("Other/Construct samples 2/Sources/fishGame.txt", "doc"),
            ]
        );
    }

    #[test]
    fn markers_match_docs_and_images_by_the_start_of_the_stem_ignoring_case() {
        for marker in [
            "LICENSE.md",
            "licence.pdf",
            "License (CC0).txt",
            "preview_large.jpg",
            "Sample2.svg",
        ] {
            let path = format!("kits/kit/{marker}");
            assert_eq!(packs(&derive([path.as_str()])), ["kits/kit"], "{marker}");
        }
        for not_marker in [
            "Readme.txt",
            "license.glb",
            "preview.fbx",
            "my_license.txt",
            "Previews/x.png",
        ] {
            let path = format!("kits/kit/{not_marker}");
            assert_eq!(
                packs(&derive([path.as_str()])),
                [] as [&str; 0],
                "{not_marker}"
            );
        }
    }

    #[test]
    fn the_outermost_marked_folder_is_the_pack() {
        let paths = [
            "kit/License.txt",
            "kit/Sprites/License.txt",
            "kit/Sprites/a.png",
        ];
        assert_eq!(packs(&derive(paths)), ["kit"]);
    }

    #[test]
    fn a_library_that_is_one_pack_has_its_root_as_the_pack() {
        let paths = ["License.txt", "Preview.png", "Models/rock.glb"];
        let structure = derive(paths);
        assert_eq!(packs(&structure), [""]);
        assert_eq!(
            hidden(&paths),
            [("License.txt", "doc"), ("Preview.png", "cover")]
        );
    }

    /// A Synty-like store export with no markers, next to two packs that have them.
    const STORE_EXPORT: [&str; 6] = [
        "Store/Nature Kit/License.txt",
        "Store/Nature Kit/tree.glb",
        "Store/Space Kit/License.txt",
        "Store/Space Kit/rocket.glb",
        "Store/POLYGON_Fantasy/Models/SM_Bld_House_01.fbx",
        "Store/POLYGON_Fantasy/Textures/PolygonFantasy_Texture_01.png",
    ];

    #[test]
    fn the_sibling_rule_makes_a_folder_without_markers_a_pack() {
        assert_eq!(
            packs(&derive(STORE_EXPORT)),
            [
                "Store/Nature Kit",
                "Store/POLYGON_Fantasy",
                "Store/Space Kit"
            ]
        );
    }

    #[test]
    fn the_sibling_rule_needs_two_packs_that_are_more_than_half_the_folders() {
        // One pack of two folders
        let one = ["a/Kit/License.txt", "a/Other/x.glb"];
        assert_eq!(packs(&derive(one)), ["a/Kit"]);
        // Two packs of four folders: half is not enough.
        let half = [
            "a/K1/License.txt",
            "a/K2/License.txt",
            "a/X/x.glb",
            "a/Y/y.glb",
        ];
        assert_eq!(packs(&derive(half)), ["a/K1", "a/K2"]);
    }

    #[test]
    fn the_sibling_rule_skips_folders_inside_a_pack_and_folders_holding_one() {
        let inside = [
            "kit/License.txt",
            "kit/A/License.txt",
            "kit/B/Preview.png",
            "kit/C/c.glb",
        ];
        assert_eq!(packs(&derive(inside)), ["kit"]);
        let holding = [
            "Store/K1/License.txt",
            "Store/K2/License.txt",
            "Store/Bundle/K3/License.txt",
            "Store/Bundle/K3/a.glb",
        ];
        assert_eq!(
            packs(&derive(holding)),
            ["Store/Bundle/K3", "Store/K1", "Store/K2"]
        );
    }

    #[test]
    fn a_category_whose_only_folder_is_not_a_pack_stays_unmarked() {
        // `Goodies/Wallpapers` is no pack, and neither is `Goodies`, whose docs stay visible.
        let paths = [
            "Goodies/Thanks.txt",
            "Goodies/kenneyPapercraft.pdf",
            "Goodies/Wallpapers/wallpaperCabin.jpg",
        ];
        assert_eq!(packs(&derive(paths)), [] as [&str; 0]);
        assert!(hidden(&paths).is_empty());
    }

    /// Each grouped file of `paths` with its main item, in input order.
    fn grouped<'a>(paths: &[&'a str]) -> Vec<(&'a str, &'a str)> {
        derive(paths.iter().copied())
            .groups
            .iter()
            .enumerate()
            .filter_map(|(i, main)| main.map(|m| (paths[i], paths[m])))
            .collect()
    }

    /// The groups of `paths` as their files, each main item first and the rest in input order.
    fn groups_of<'a>(paths: &[&'a str]) -> Vec<Vec<&'a str>> {
        let mut groups: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (file, main) in grouped(paths) {
            let files = groups.entry(main).or_insert_with(|| vec![main]);
            if file != main {
                files.push(file);
            }
        }
        groups.into_values().collect()
    }

    #[test]
    fn a_kenney_kit_model_is_one_glb_item_and_so_are_its_texture_copies() {
        let paths = [
            "3D assets/Car Kit/License.txt",
            "3D assets/Car Kit/Models/FBX format/sedan.fbx",
            "3D assets/Car Kit/Models/FBX format/Textures/colormap.png",
            "3D assets/Car Kit/Models/GLB format/sedan.glb",
            "3D assets/Car Kit/Models/GLB format/sedan-sports.glb",
            "3D assets/Car Kit/Models/GLB format/Textures/colormap.png",
            "3D assets/Car Kit/Models/OBJ format/sedan.obj",
            "3D assets/Car Kit/Models/OBJ format/sedan-sports.obj",
            "3D assets/Car Kit/Models/OBJ format/Textures/colormap.png",
            "3D assets/Car Kit/Previews/sedan.png",
        ];
        assert_eq!(
            groups_of(&paths),
            [
                vec![
                    "3D assets/Car Kit/Models/GLB format/Textures/colormap.png",
                    "3D assets/Car Kit/Models/FBX format/Textures/colormap.png",
                    "3D assets/Car Kit/Models/OBJ format/Textures/colormap.png",
                ],
                vec![
                    "3D assets/Car Kit/Models/GLB format/sedan-sports.glb",
                    "3D assets/Car Kit/Models/OBJ format/sedan-sports.obj",
                ],
                vec![
                    "3D assets/Car Kit/Models/GLB format/sedan.glb",
                    "3D assets/Car Kit/Models/FBX format/sedan.fbx",
                    "3D assets/Car Kit/Models/OBJ format/sedan.obj",
                ],
            ]
        );

        // Every format folder counts every model it holds a copy of, and `Models` counts each
        // model and the texture once. The render is the sedan's hidden bundled preview.
        let structure = derive(paths);
        let count = |path: &str| structure.folders[path].item_count;
        assert_eq!(count("3D assets/Car Kit/Models/GLB format"), 3);
        assert_eq!(count("3D assets/Car Kit/Models/OBJ format"), 3);
        assert_eq!(count("3D assets/Car Kit/Models/FBX format"), 2);
        assert_eq!(count("3D assets/Car Kit/Models/FBX format/Textures"), 1);
        assert_eq!(count("3D assets/Car Kit/Models"), 3);
        assert_eq!(count("3D assets/Car Kit"), 3);
        assert_eq!(count(""), 3);
        // Only the folders holding a member without its main item split groups.
        let split: Vec<(&str, i64)> = structure
            .folders
            .iter()
            .filter(|(_, f)| f.split_groups > 0)
            .map(|(path, f)| (path.as_str(), f.split_groups))
            .collect();
        assert_eq!(
            split,
            [
                ("3D assets/Car Kit/Models/FBX format", 2),
                ("3D assets/Car Kit/Models/FBX format/Textures", 1),
                ("3D assets/Car Kit/Models/OBJ format", 3),
                ("3D assets/Car Kit/Models/OBJ format/Textures", 1),
            ]
        );
    }

    #[test]
    fn quaternius_models_group_across_their_per_format_folders() {
        let paths = [
            "Ultimate Spaceships/Bob/Blend/Bob.blend",
            "Ultimate Spaceships/Bob/FBX/Bob.fbx",
            "Ultimate Spaceships/Bob/OBJ/Bob.obj",
            "Ultimate Spaceships/Bob/glTF/Bob.gltf",
            "Ultimate Spaceships/Bob/Textures/Bob_Blue.png",
            "Ultimate Spaceships/Spitfire/FBX/Spitfire.fbx",
            "Ultimate Spaceships/Spitfire/glTF/Spitfire.gltf",
        ];
        assert_eq!(
            groups_of(&paths),
            [
                vec![
                    "Ultimate Spaceships/Bob/glTF/Bob.gltf",
                    "Ultimate Spaceships/Bob/Blend/Bob.blend",
                    "Ultimate Spaceships/Bob/FBX/Bob.fbx",
                    "Ultimate Spaceships/Bob/OBJ/Bob.obj",
                ],
                vec![
                    "Ultimate Spaceships/Spitfire/glTF/Spitfire.gltf",
                    "Ultimate Spaceships/Spitfire/FBX/Spitfire.fbx",
                ],
            ]
        );
    }

    #[test]
    fn kaykit_groups_its_gltf_and_unity_fbx_folders_and_their_textures() {
        let paths = [
            "KayKit City Builder Bits/Assets/fbx(unity)/building_A.fbx",
            "KayKit City Builder Bits/Assets/fbx(unity)/citybits_texture.png",
            "KayKit City Builder Bits/Assets/gltf/building_A.gltf",
            "KayKit City Builder Bits/Assets/gltf/citybits_texture.png",
            "KayKit City Builder Bits/Assets/obj/building_A.obj",
            "KayKit City Builder Bits/Assets/obj/citybits_texture.png",
            "KayKit City Builder Bits/Assets/FBX (Unity)/car_taxi.fbx",
            "KayKit City Builder Bits/Assets/gltf/car_taxi.gltf",
        ];
        assert_eq!(
            groups_of(&paths),
            [
                vec![
                    "KayKit City Builder Bits/Assets/gltf/building_A.gltf",
                    "KayKit City Builder Bits/Assets/fbx(unity)/building_A.fbx",
                    "KayKit City Builder Bits/Assets/obj/building_A.obj",
                ],
                vec![
                    "KayKit City Builder Bits/Assets/gltf/car_taxi.gltf",
                    "KayKit City Builder Bits/Assets/FBX (Unity)/car_taxi.fbx",
                ],
                vec![
                    "KayKit City Builder Bits/Assets/gltf/citybits_texture.png",
                    "KayKit City Builder Bits/Assets/fbx(unity)/citybits_texture.png",
                    "KayKit City Builder Bits/Assets/obj/citybits_texture.png",
                ],
            ]
        );
    }

    #[test]
    fn a_poly_haven_asset_groups_its_formats_side_by_side_but_not_its_maps() {
        let paths = [
            "Poly Haven/wooden_table_02/wooden_table_02_1k.blend",
            "Poly Haven/wooden_table_02/wooden_table_02_1k.fbx",
            "Poly Haven/wooden_table_02/wooden_table_02_1k.gltf",
            "Poly Haven/wooden_table_02/textures/wooden_table_02_diff_1k.jpg",
            "Poly Haven/wooden_table_02/textures/wooden_table_02_nor_gl_1k.jpg",
            "Poly Haven/wooden_table_02/wooden_table_02_1k.png",
        ];
        assert_eq!(
            groups_of(&paths),
            [vec![
                "Poly Haven/wooden_table_02/wooden_table_02_1k.gltf",
                "Poly Haven/wooden_table_02/wooden_table_02_1k.blend",
                "Poly Haven/wooden_table_02/wooden_table_02_1k.fbx",
            ]]
        );
    }

    #[test]
    fn glb_copies_in_a_gltf_folder_lead_their_groups_and_ties_break_by_path() {
        // Retro Fantasy Kit's `GLTF format/` holds `.glb` files.
        let retro = [
            "3D assets/Retro Fantasy Kit/Models/FBX format/wall.fbx",
            "3D assets/Retro Fantasy Kit/Models/GLTF format/wall.glb",
            "3D assets/Retro Fantasy Kit/Models/OBJ format/wall.obj",
        ];
        assert_eq!(
            grouped(&retro)
                .iter()
                .map(|(_, main)| *main)
                .collect::<Vec<_>>(),
            [retro[1]; 3]
        );
        // Two `.glb` copies: the path that sorts first ignoring case leads.
        let tied = [
            "kit/gltf format/wall.glb",
            "kit/GLB format/wall.glb",
            "kit/OBJ format/wall.obj",
        ];
        assert_eq!(
            grouped(&tied)
                .iter()
                .map(|(_, main)| *main)
                .collect::<Vec<_>>(),
            [tied[1]; 3]
        );
    }

    #[test]
    fn files_group_only_when_their_names_and_folders_match_once_format_folders_are_removed() {
        let paths = [
            // Different stems, or a different folder once the format folder is taken out
            "kit/Models/GLB format/rock.glb",
            "kit/Models/GLB format/rock_large.glb",
            "kit/Other/FBX format/rock.fbx",
            // Images with one stem but outside format folders, and a texture and a model
            // sharing a stem
            "kit/Tiles/tile_0000.png",
            "kit/Tiles/Characters/tile_0000.png",
            "kit/Models/GLB format/crate.glb",
            "kit/Models/GLB format/crate.png",
            // Copies under format folders with different extensions
            "kit/Models/GLB format/Textures/colormap.png",
            "kit/Models/OBJ format/Textures/colormap.jpg",
            // A folder named like a format, but not a format folder
            "kit/GLB stuff/tree.glb",
            "kit/tree.obj",
        ];
        assert_eq!(grouped(&paths), []);
    }

    #[test]
    fn format_folder_names_ignore_case_and_a_format_or_files_suffix() {
        for name in [
            "GLB format",
            "glTF",
            "FBX files",
            "fbx(unity)",
            "FBX (Unity)",
            "Blend",
            "dae format",
            "STL",
            "3ds",
            "USD",
            "usdz files",
            "PLY format",
            "OBJ",
        ] {
            assert!(format_folder(name).is_some(), "{name}");
        }
        for name in [
            "Models",
            "GLB formats",
            "fbx unity",
            "Source",
            "glb-format",
            "PNG",
        ] {
            assert!(format_folder(name).is_none(), "{name}");
        }
    }

    #[test]
    fn support_files_never_group() {
        // Two marked folders' previews under format folders: each is a support file, so neither
        // groups with the other.
        let paths = [
            "kit/License.txt",
            "kit/GLB format/License.txt",
            "kit/GLB format/Preview.png",
            "kit/OBJ format/License.txt",
            "kit/OBJ format/Preview.png",
            "kit/GLB format/rock.glb",
            "kit/OBJ format/rock.obj",
        ];
        assert_eq!(hidden(&paths).len(), 5);
        assert_eq!(
            groups_of(&paths),
            [vec!["kit/GLB format/rock.glb", "kit/OBJ format/rock.obj"]]
        );
    }

    /// Each item of `paths` that has a bundled preview, with its render, in input order.
    fn bundled<'a>(paths: &[&'a str]) -> Vec<(&'a str, &'a str)> {
        derive(paths.iter().copied())
            .bundled_previews
            .iter()
            .enumerate()
            .filter_map(|(i, render)| render.map(|r| (paths[i], paths[r])))
            .collect()
    }

    #[test]
    fn a_kenney_render_is_its_models_hidden_bundled_preview() {
        let paths = [
            "3D assets/Car Kit/License.txt",
            "3D assets/Car Kit/Preview.png",
            "3D assets/Car Kit/Models/GLB format/sedan.glb",
            "3D assets/Car Kit/Models/FBX format/sedan.fbx",
            "3D assets/Car Kit/Models/OBJ format/sedan.obj",
            "3D assets/Car Kit/Models/GLB format/Textures/colormap.png",
            "3D assets/Car Kit/Models/OBJ format/Textures/colormap.png",
            "3D assets/Car Kit/Previews/sedan.png",
            // A render with no matching model
            "3D assets/Car Kit/Previews/truck.png",
        ];
        // The GLB leads the sedan and records the render, which no member does.
        assert_eq!(
            bundled(&paths),
            [(
                "3D assets/Car Kit/Models/GLB format/sedan.glb",
                "3D assets/Car Kit/Previews/sedan.png"
            )]
        );
        assert_eq!(
            hidden(&paths),
            [
                ("3D assets/Car Kit/License.txt", "doc"),
                ("3D assets/Car Kit/Preview.png", "cover"),
                ("3D assets/Car Kit/Previews/sedan.png", "model_preview"),
            ]
        );
        // The unmatched render stays an ordinary image on its own, and the matched one joins no
        // group.
        let structure = derive(paths);
        assert_eq!(structure.groups[8], None);
        assert_eq!(structure.groups[7], None);
        let previews = &structure.folders["3D assets/Car Kit/Previews"];
        assert_eq!((previews.item_count, previews.hidden_count), (1, 1));
        assert_eq!(structure.folders["3D assets/Car Kit"].item_count, 3);
    }

    #[test]
    fn a_render_matches_a_model_below_its_folders_parent_by_stem_ignoring_case() {
        let paths = [
            // `Preview/` and `renders/` hold renders too, and a model on its own records its own.
            "kit/Preview/Rock.PNG",
            "kit/deep/models/rock.glb",
            "kit/renders/tree.jpg",
            "kit/tree.fbx",
            // A model beside the render's folder's parent, not below it
            "a/Previews/barrel.png",
            "b/barrel.glb",
            // Not a render folder, and an image named like a model but no image
            "kit/Images/rock.png",
            "kit/Previews/notes.txt",
            "kit/notes.obj",
        ];
        assert_eq!(
            bundled(&paths),
            [
                ("kit/deep/models/rock.glb", "kit/Preview/Rock.PNG"),
                ("kit/tree.fbx", "kit/renders/tree.jpg"),
            ]
        );
        let roles: Vec<&str> = hidden(&paths).into_iter().map(|(p, _)| p).collect();
        assert_eq!(roles, ["kit/Preview/Rock.PNG", "kit/renders/tree.jpg"]);
    }

    #[test]
    fn a_render_at_the_library_root_matches_every_model_and_one_item_takes_the_first() {
        let paths = [
            "Previews/rock.png",
            "Renders/rock.png",
            "kit/rock.glb",
            "other/rock.glb",
        ];
        // Both models get a render, and each takes the one whose path sorts first.
        assert_eq!(
            bundled(&paths),
            [
                ("kit/rock.glb", "Previews/rock.png"),
                ("other/rock.glb", "Previews/rock.png"),
            ]
        );
        assert_eq!(hidden(&paths).len(), 2);
    }

    #[test]
    fn an_image_the_webview_cant_display_is_no_render() {
        // A render with no thumbnail shows as itself, so it must be one the webview decodes.
        let paths = [
            "kit/Renders/rock.dds",
            "kit/Previews/tree.tga",
            "kit/Previews/barrel.webp",
            "kit/rock.glb",
            "kit/tree.glb",
            "kit/barrel.glb",
        ];
        assert_eq!(
            bundled(&paths),
            [("kit/barrel.glb", "kit/Previews/barrel.webp")]
        );
        assert_eq!(
            hidden(&paths),
            [("kit/Previews/barrel.webp", "model_preview")]
        );
    }

    #[test]
    fn an_image_that_is_already_a_support_file_is_no_render() {
        // `Previews/` is marked by its `Preview.png`, which stays a cover.
        let paths = [
            "kit/License.txt",
            "kit/Previews/Preview.png",
            "kit/preview.glb",
        ];
        assert_eq!(bundled(&paths), []);
        assert_eq!(
            hidden(&paths),
            [
                ("kit/License.txt", "doc"),
                ("kit/Previews/Preview.png", "cover"),
            ]
        );
    }

    #[test]
    fn a_packs_cover_is_its_first_preview_then_sample_then_image_then_visible_file() {
        let cover = |paths: &[&str]| info(&derive(paths.iter().copied()), "kit").cover.clone();
        let base = [
            "kit/License.txt",
            "kit/Models/rock.glb",
            "kit/Models/barrel.glb",
        ];
        let with = |extra: &[&'static str]| [&base[..], extra].concat();
        // With no image, the first visible file: a model, whose thumbnail shows.
        assert_eq!(cover(&base).as_deref(), Some("kit/Models/barrel.glb"));
        assert_eq!(cover(&["kit/License.txt"]), None);
        assert_eq!(
            cover(&with(&["kit/Tiles/b.png", "kit/Tiles/a.png"])).as_deref(),
            Some("kit/Tiles/a.png")
        );
        assert_eq!(
            cover(&with(&[
                "kit/Tiles/a.png",
                "kit/SampleB.png",
                "kit/SampleA.png"
            ]))
            .as_deref(),
            Some("kit/SampleA.png")
        );
        assert_eq!(
            cover(&with(&[
                "kit/Sample.png",
                "kit/Preview2.png",
                "kit/Preview.png"
            ]))
            .as_deref(),
            Some("kit/Preview.png")
        );
        // A preview that is no cover image (in `Previews/`, or a doc) doesn't count as one.
        assert_eq!(
            cover(&with(&[
                "kit/Previews/rock.png",
                "kit/Preview.pdf",
                "kit/Sample.png"
            ]))
            .as_deref(),
            Some("kit/Sample.png")
        );
        // A marked folder inside the pack holds cover images too.
        assert_eq!(
            cover(&with(&["kit/Sources/Preview.png", "kit/Tiles/a.png"])).as_deref(),
            Some("kit/Sources/Preview.png")
        );
        // A preview wins over a sample that sorts before it.
        assert_eq!(
            cover(&with(&["kit/Sample.png", "kit/Sources/Preview.png"])).as_deref(),
            Some("kit/Sources/Preview.png")
        );
    }

    #[test]
    fn each_pack_takes_the_cover_and_license_from_its_own_files() {
        let structure = derive(KENNEY_3D_KIT);
        let car = info(&structure, "3D assets/Car Kit");
        assert_eq!(car.cover.as_deref(), Some("3D assets/Car Kit/Preview.png"));
        assert_eq!(
            car.license.as_ref().map(|l| l.path.as_str()),
            Some("3D assets/Car Kit/License.txt")
        );
        // A pack whose only file is its license has no cover.
        let holiday = info(&structure, "3D assets/Holiday Kit");
        assert_eq!(
            (holiday.name.as_str(), &holiday.cover),
            ("Holiday Kit", &None)
        );
        assert!(structure.folders["3D assets"].pack.is_none());
        // Only a license directly in the pack names it.
        let inner = derive(["kit/Preview.png", "kit/Sprites/License.txt"]);
        assert_eq!(info(&inner, "kit").license, None);
    }

    /// A license that says `kind` and has a Kenney header naming `name`, at version 1.0.
    fn kenney(name: &str, kind: LicenseKind) -> LicenseText {
        LicenseText {
            kind,
            header: Some(PackHeader {
                name: name.to_string(),
                version: Some("1.0".to_string()),
                author: Some("Kenney".to_string()),
            }),
        }
    }

    #[test]
    fn a_packs_name_is_its_license_header_name_else_its_folder_name() {
        let structure = super::derive(KENNEY_3D_KIT.map(|p| (p, 1)), |path| {
            if path == "3D assets/Car Kit/License.txt" {
                kenney("Car Kit Deluxe", LicenseKind::Cc0)
            } else {
                LicenseText::default()
            }
        });
        let car = info(&structure, "3D assets/Car Kit");
        assert_eq!(car.name, "Car Kit Deluxe");
        assert_eq!(
            car.license.as_ref().map(|l| &l.text),
            Some(&kenney("Car Kit Deluxe", LicenseKind::Cc0))
        );
        assert_eq!(
            info(&structure, "3D assets/Holiday Kit").name,
            "Holiday Kit"
        );
        // The folder keeps its own name in the tree.
        assert_eq!(structure.folders["3D assets/Car Kit"].name, "Car Kit");
    }

    #[test]
    fn a_packs_gallery_is_every_cover_image_by_path() {
        let structure = derive(KENNEY_3D_KIT);
        assert_eq!(
            info(&structure, "3D assets/Car Kit").gallery,
            [
                "3D assets/Car Kit/Preview.png",
                "3D assets/Car Kit/Sample.png"
            ]
        );
        assert!(info(&structure, "3D assets/Holiday Kit").gallery.is_empty());
        // Cover images in a marked folder inside the pack join it; a render, a doc named
        // `Preview` and an image under another name don't.
        let structure = derive([
            "kit/License.txt",
            "kit/Sample2.png",
            "kit/Sources/Preview.jpg",
            "kit/Models/rock.glb",
            "kit/Previews/rock.png",
            "kit/Preview.pdf",
            "kit/Tiles/tile.png",
        ]);
        assert_eq!(
            info(&structure, "kit").gallery,
            ["kit/Sample2.png", "kit/Sources/Preview.jpg"]
        );
    }

    #[test]
    fn a_pack_counts_every_file_below_it_by_kind_with_their_total_size() {
        let sizes = [10, 200, 300, 4_000, 50, 6_000, 7_000, 80, 9];
        let structure = super::derive(KENNEY_3D_KIT.into_iter().zip(sizes), |_| {
            LicenseText::default()
        });
        let car = info(&structure, "3D assets/Car Kit");
        // Support files and every group member count, the render and the license included.
        assert_eq!(
            car.kinds,
            BTreeMap::from([
                ("3d_model".to_string(), 3),
                ("document".to_string(), 1),
                ("texture".to_string(), 4),
            ])
        );
        assert_eq!(
            car.size_bytes,
            10 + 200 + 300 + 4_000 + 50 + 6_000 + 7_000 + 80
        );
        let holiday = info(&structure, "3D assets/Holiday Kit");
        assert_eq!((holiday.kinds.len(), holiday.size_bytes), (1, 9));
        // Files at any depth count, those in a marked folder inside the pack included.
        let structure = super::derive(
            [
                ("kit/License.txt", 1),
                ("kit/Extra/License.txt", 2),
                ("kit/Extra/a/b.glb", 4),
            ],
            |_| LicenseText::default(),
        );
        assert_eq!(info(&structure, "kit").size_bytes, 7);
        // A library that is one pack counts each file once, those at its root included.
        let structure = super::derive(
            [("License.txt", 1), ("rock.png", 2), ("Tiles/a.png", 4)],
            |_| LicenseText::default(),
        );
        let root = info(&structure, "");
        assert_eq!((root.kinds.values().sum::<i64>(), root.size_bytes), (3, 7));
    }

    /// A Kenney `License.txt` as Kenney ships it, from the bench library's Car Kit.
    const KENNEY_LICENSE: &str = "\u{feff}\t\r\n\r\n\tCar Kit (3.1)\r\n\r\n\tCreated/distributed by Kenney (www.kenney.nl)\r\n\tCreation date: 02-04-2026 14:03\r\n\t\r\n\t\t\t------------------------------\r\n\r\n\tLicense: (Creative Commons Zero, CC0)\r\n\thttp://creativecommons.org/publicdomain/zero/1.0/\r\n\r\n\tYou can use this content for personal, educational, and commercial purposes.\r\n\r\n\tSupport by crediting 'Kenney' or 'www.kenney.nl' (this is not a requirement)\r\n";
    /// A KayKit `LICENSE.txt`: CC0, with a header naming its own author.
    const KAYKIT_LICENSE: &str = "KayKit : Adventurers (1.0)\n\nCreated and distributed by Kay Lousberg (www.kaylousberg.com)\n\n------------------------------\n\nLicense: (Creative Commons Zero, CC0)\nhttp://creativecommons.org/publicdomain/zero/1.0/\n\nThis content is free to use in personal, educational and commercial projects.\n";
    const MIT_LICENSE: &str = "MIT License\n\nCopyright (c) 2024 Some Author\n\nPermission is hereby granted, free of charge, to any person obtaining a copy\nof this software and associated documentation files (the \"Software\"), to deal\nin the Software without restriction, including without limitation the rights\nto use, copy, modify, merge, publish, distribute, sublicense, and/or sell\ncopies of the Software.\n\nTHE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND.\n";

    #[test]
    fn a_kenney_license_gives_the_name_version_author_and_cc0() {
        assert_eq!(
            parse_license(KENNEY_LICENSE),
            LicenseText {
                kind: LicenseKind::Cc0,
                header: Some(PackHeader {
                    name: "Car Kit".to_string(),
                    version: Some("3.1".to_string()),
                    author: Some("Kenney".to_string()),
                }),
            }
        );
        // The older byline, with no version
        assert_eq!(
            kenney_header("Top-down Tanks Remastered\n\tby  Kenney Vleugels (Kenney.nl)\n"),
            Some(PackHeader {
                name: "Top-down Tanks Remastered".to_string(),
                version: None,
                author: Some("Kenney Vleugels".to_string()),
            })
        );
    }

    #[test]
    fn a_kaykit_or_mit_license_has_its_kind_and_no_kenney_header() {
        assert_eq!(
            parse_license(KAYKIT_LICENSE),
            LicenseText {
                kind: LicenseKind::Cc0,
                header: None,
            }
        );
        assert_eq!(
            parse_license(MIT_LICENSE),
            LicenseText {
                kind: LicenseKind::Mit,
                header: None,
            }
        );
        // The MIT text without its title
        assert_eq!(
            license_kind(&MIT_LICENSE.replace("MIT License\n", "")),
            LicenseKind::Mit
        );
    }

    #[test]
    fn a_license_is_recognized_by_its_name_or_url_else_unknown() {
        for (text, kind) in [
            ("Licensed under CC0 1.0 Universal", LicenseKind::Cc0),
            (
                "https://creativecommons.org/licenses/by/4.0/",
                LicenseKind::CcBy,
            ),
            ("License: CC-BY 4.0. Credit the author.", LicenseKind::CcBy),
            (
                "Creative Commons Attribution 4.0 International",
                LicenseKind::CcBy,
            ),
            (
                "https://creativecommons.org/licenses/by-sa/4.0/",
                LicenseKind::CcBySa,
            ),
            ("Released as CC BY-SA 3.0", LicenseKind::CcBySa),
            (
                "Creative Commons Attribution-ShareAlike 4.0",
                LicenseKind::CcBySa,
            ),
            (
                "Apache License\nVersion 2.0, January 2004",
                LicenseKind::Apache2,
            ),
            ("SPDX-License-Identifier: Apache-2.0", LicenseKind::Apache2),
            (
                "http://www.apache.org/licenses/LICENSE-2.0",
                LicenseKind::Apache2,
            ),
            ("SIL OPEN FONT LICENSE Version 1.1", LicenseKind::Ofl),
            ("https://openfontlicense.org", LicenseKind::Ofl),
            // The license named first wins.
            ("CC-BY 4.0, or else MIT license", LicenseKind::CcBy),
            // Others it doesn't recognize
            (
                "Creative Commons Attribution-NonCommercial 4.0",
                LicenseKind::Unknown,
            ),
            (
                "https://creativecommons.org/licenses/by-nc/4.0/",
                LicenseKind::Unknown,
            ),
            ("CC BY-NC-SA 4.0", LicenseKind::Unknown),
            ("Apache License 1.1", LicenseKind::Unknown),
            (
                "Creative Commons Attribution Non Commercial 4.0",
                LicenseKind::Unknown,
            ),
            (
                "Creative Commons Attribution, Non-Commercial (CC BY-NC 4.0)",
                LicenseKind::Unknown,
            ),
            (
                "Licensed under Creative Commons Attribution (CC BY-NC 4.0)",
                LicenseKind::Unknown,
            ),
            (
                "Creative Commons Attribution-NoDerivatives 4.0",
                LicenseKind::Unknown,
            ),
            ("CC BY ND 4.0", LicenseKind::Unknown),
            // A name inside another word is no name.
            (
                "You may not permit licensees to sell the assets.",
                LicenseKind::Unknown,
            ),
            (
                "Do not submit licensed assets to stores.",
                LicenseKind::Unknown,
            ),
            (
                "There is a limit license holders must respect.",
                LicenseKind::Unknown,
            ),
            ("Use the arcc0 tool.", LicenseKind::Unknown),
            (
                "Creative Commons Attribution Share Alike 4.0",
                LicenseKind::CcBySa,
            ),
            (
                "All rights reserved. Do not redistribute.",
                LicenseKind::Unknown,
            ),
            ("", LicenseKind::Unknown),
        ] {
            assert_eq!(license_kind(text), kind, "{text:?}");
        }
    }

    #[test]
    fn the_kenney_header_names_the_pack_without_its_version() {
        let header = |text: &str| kenney_header(text).map(|h| (h.name, h.version));
        let kenney = "\u{feff}\r\n\tCar Kit (3.1)\r\n\r\n\tCreated/distributed by Kenney (www.kenney.nl)\r\n\tCreation date: 02-04-2026\r\n";
        assert_eq!(
            header(kenney),
            Some(("Car Kit".to_string(), Some("3.1".to_string())))
        );
        for (title, name, version) in [
            ("Input Prompts (1.5A)", "Input Prompts", Some("1.5A")),
            (
                "Space Kit (1.2) Exclusive",
                "Space Kit Exclusive",
                Some("1.2"),
            ),
            ("Sketch Desert 1.0", "Sketch Desert 1.0", None),
            ("Generic Items #1", "Generic Items #1", None),
            ("Platformer Pack (Redux)", "Platformer Pack (Redux)", None),
        ] {
            let text = format!("{title}\n\nCreated/distributed by Kenney (www.kenney.nl)\n");
            assert_eq!(
                header(&text),
                Some((name.to_string(), version.map(str::to_string))),
                "{title}"
            );
        }
        // The older byline
        assert_eq!(
            header("Roguelike Indoor pack\nby Kenney Vleugels (Kenney.nl)\n---"),
            Some(("Roguelike Indoor pack".to_string(), None))
        );
        // Another license's first line names no pack.
        for text in [
            MIT_LICENSE,
            KAYKIT_LICENSE,
            "Creative Commons Zero v1.0 Universal\n\nCC0 1.0 Universal\n",
            "Car Kit (3.1)",
            "",
            // A byline that doesn't name Kenney
            "License Agreement\n\nBy downloading this pack you agree to these terms.\n",
            "Space Pack\n\nCreated/distributed by Someone Else\n",
        ] {
            assert_eq!(header(text), None, "{text:?}");
        }
    }

    #[test]
    fn ui_pack_sprites_stack_across_their_scale_and_vector_copies_but_not_their_colors() {
        let paths = [
            UI_PACK.to_vec(),
            vec![
                "UI assets/UI Pack/PNG/Red/Default/button_rectangle.png",
                "UI assets/UI Pack/PNG/Red/Double/button_rectangle.png",
                "UI assets/UI Pack/Vector/Red/button_rectangle.svg",
            ],
        ]
        .concat();
        assert_eq!(
            groups_of(&paths),
            [
                vec![
                    "UI assets/UI Pack/PNG/Blue/Default/button_rectangle.png",
                    "UI assets/UI Pack/PNG/Blue/Double/button_rectangle.png",
                    "UI assets/UI Pack/Vector/Blue/button_rectangle.svg",
                ],
                vec![
                    "UI assets/UI Pack/PNG/Red/Default/button_rectangle.png",
                    "UI assets/UI Pack/PNG/Red/Double/button_rectangle.png",
                    "UI assets/UI Pack/Vector/Red/button_rectangle.svg",
                ],
            ]
        );
        // Each color's folders count its stack once, next to the spritesheet, and the vector
        // folder holds the stack without its main item.
        let structure = derive(paths.iter().copied());
        let count = |path: &str| structure.folders[path].item_count;
        assert_eq!(count("UI assets/UI Pack"), 3);
        assert_eq!(count("UI assets/UI Pack/PNG"), 2);
        assert_eq!(count("UI assets/UI Pack/Vector/Blue"), 1);
        assert_eq!(
            structure.folders["UI assets/UI Pack/Vector"].split_groups,
            2
        );
        assert_eq!(structure.folders["UI assets/UI Pack/PNG"].split_groups, 0);
    }

    #[test]
    fn kenney_2d_sprites_stack_in_the_current_and_the_old_style() {
        // The current style ships each sprite once, so nothing stacks, and two tiles that share a
        // name in different folders are different pictures.
        assert_eq!(groups_of(&KENNEY_2D), [] as [Vec<&str>; 0]);
        // The old style ships PNG and vector copies under matching folders, and Holiday Pack's
        // `Default size/` and `Retina/` copies.
        let old = [
            "2D assets/Abstract Platformer/License.txt",
            "2D assets/Abstract Platformer/Preview.png",
            "2D assets/Abstract Platformer/PNG/Players/Player Blue/playerBlue_walk1.png",
            "2D assets/Abstract Platformer/PNG/Players/Player Green/playerGreen_walk1.png",
            "2D assets/Abstract Platformer/Vector/Players/Player Blue/playerBlue_walk1.svg",
            "2D assets/Abstract Platformer/Spritesheet/spritesheet_complete.png",
            "2D assets/Abstract Platformer/Vector/vector_complete.svg",
        ];
        assert_eq!(
            groups_of(&old),
            [vec![
                "2D assets/Abstract Platformer/PNG/Players/Player Blue/playerBlue_walk1.png",
                "2D assets/Abstract Platformer/Vector/Players/Player Blue/playerBlue_walk1.svg",
            ]]
        );
        assert_eq!(
            groups_of(&HOLIDAY_PACK),
            [vec![
                "2D assets/Holiday Pack 2016/PNG/RTS pack/Default size/tree.png",
                "2D assets/Holiday Pack 2016/PNG/RTS pack/Retina/tree.png",
            ]]
        );
    }

    #[test]
    fn same_named_tiles_in_different_folders_stay_apart() {
        let paths = [
            "2D assets/Pixel Platformer/Tiles/tile_0000.png",
            "2D assets/Pixel Platformer/Tiles/Characters/tile_0000.png",
            "2D assets/Tiny Town/Tiles/tile_0000.png",
            "2D assets/Tiny Dungeon/Tilemap/tile_0000.png",
        ];
        assert_eq!(grouped(&paths), []);
        assert_eq!(derive(paths).folders[""].item_count, 4);
    }

    #[test]
    fn scale_folders_stack_by_name_note_and_ending_but_style_and_color_folders_do_not() {
        let stacked: [&[&str]; 8] = [
            &["p/Default/a.png", "p/Double/a.png", "p/Retina/a.png"],
            &["p/1x/a.png", "p/2x/a.png", "p/3x/a.png", "p/4x/a.png"],
            &["p/Default (64px)/a.png", "p/Double (128px)/a.png"],
            &["p/Default/a.png", "p/Large (2×)/a.png", "p/Vector/a.svg"],
            &["p/PNG/Sprites/a.png", "p/PNG/Sprites X2/a.png"],
            &[
                "p/PNG/Poses/a.png",
                "p/PNG/Poses HD/a.png",
                "p/HD/Poses/a.png",
            ],
            &[
                "p/PNG/Dark/a.png",
                "p/PNG/Dark (2×)/a.png",
                "p/SVG/Dark/a.svg",
            ],
            &[
                "p/a.png",
                "p/x2/a.png",
                "p/Vectors/a.svg",
                "p/Default size/A.PNG",
            ],
        ];
        for paths in stacked {
            assert_eq!(groups_of(paths).len(), 1, "{paths:?}");
            assert_eq!(grouped(paths).len(), paths.len(), "{paths:?}");
        }
        let apart: [&[&str]; 6] = [
            &["p/PNG/Dark/a.png", "p/PNG/Light/a.png"],
            &["p/PNG/Blue/a.png", "p/PNG/Red/a.png"],
            &["p/Tiles (Black)/a.png", "p/Tiles (White)/a.png"],
            &["p/Fantasy (128px)/a.png", "p/Space (128px)/a.png"],
            &["p/PNG/Dark (2×)/a.png", "p/PNG/Light/a.png"],
            &["p/Small tiles/a.png", "p/Large tiles/a.png"],
        ];
        for paths in apart {
            assert_eq!(grouped(paths), [], "{paths:?}");
        }
    }

    #[test]
    fn a_stack_is_led_by_a_png_then_by_the_default_scale() {
        let main = |paths: &[&'static str]| groups_of(paths)[0][0];
        // A PNG before a vector or a JPEG, even a larger one
        assert_eq!(
            main(&["p/Vector/a.svg", "p/Default/a.jpg", "p/Double/a.png"]),
            "p/Double/a.png"
        );
        // The default scale before a larger one, whatever their paths
        assert_eq!(
            main(&["p/Double/a.png", "p/Default/a.png"]),
            "p/Default/a.png"
        );
        assert_eq!(main(&["p/2x/a.png", "p/1x/a.png"]), "p/1x/a.png");
        assert_eq!(main(&["p/Retina/a.png", "p/a.png"]), "p/a.png");
        assert_eq!(
            main(&["p/Sprites X2/a.png", "p/Sprites/a.png"]),
            "p/Sprites/a.png"
        );
        // Two copies at the default scale: the path that sorts first ignoring case
        assert_eq!(main(&["p/PNG/a.png", "p/Default/a.png"]), "p/Default/a.png");
    }

    #[test]
    fn grouped_files_support_files_and_models_never_stack() {
        let paths = [
            // A pack's covers in a scale folder and beside it
            "kit/License.txt",
            "kit/Preview.png",
            "kit/Default/Preview.png",
            // A texture's copies under model format folders are a format group already. A scale
            // copy beside the GLB one shares its stack folder, but doesn't join it.
            "kit/Models/GLB format/Textures/colormap.png",
            "kit/Models/OBJ format/Textures/colormap.png",
            "kit/Models/GLB format/Textures/Double/colormap.png",
            // A render is its model's bundled preview
            "kit/Models/GLB format/barrel.glb",
            "kit/Previews/barrel.png",
            "kit/Default/barrel.png",
            // A model and a sprite that share a name
            "kit/PNG/crate.png",
            "kit/crate.glb",
        ];
        assert_eq!(
            groups_of(&paths),
            [vec![
                "kit/Models/GLB format/Textures/colormap.png",
                "kit/Models/OBJ format/Textures/colormap.png",
            ]]
        );
        assert_eq!(derive(paths).groups[5], None, "the scale copy stacks");
    }

    /// Every `folders` row, every asset's support role, the main item's path of every grouped
    /// asset, and every bundled preview, in path order.
    #[derive(Debug, PartialEq)]
    pub(crate) struct DerivedRows {
        pub folders: Vec<FolderRow>,
        pub roles: Vec<(String, Option<String>)>,
        pub groups: Vec<(String, Option<String>)>,
        pub bundled_previews: Vec<(String, String)>,
    }

    /// What the pass has written to `pool`, for comparing a library with a fresh pass. A license's
    /// parser, size and mtime are left out: they record when the pass last read the file, which
    /// depends on the files on disk, not on the derived structure.
    pub(crate) async fn derived_rows(pool: &Pool<Sqlite>) -> DerivedRows {
        let rows: Vec<FolderRow> = sqlx::query_as(
            "SELECT path, parent_path, name, is_pack, item_count, hidden_count, split_groups,
                    pack_json
             FROM folders ORDER BY path",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        let folders = rows
            .into_iter()
            .map(
                |(path, parent, name, is_pack, items, hidden, split, pack_json)| {
                    let pack_json = pack_json.map(|json| {
                        let mut pack = parse_pack(Some(&json)).expect("pack_json holds pack info");
                        if let Some(license) = pack.license.as_mut() {
                            (license.parser, license.size_bytes, license.modified_at) = (0, 0, 0);
                        }
                        serde_json::to_string(&pack).unwrap()
                    });
                    (path, parent, name, is_pack, items, hidden, split, pack_json)
                },
            )
            .collect();
        let roles = sqlx::query_as("SELECT file_path, support_role FROM assets ORDER BY file_path")
            .fetch_all(pool)
            .await
            .unwrap();
        let groups = sqlx::query_as(
            "SELECT a.file_path, m.file_path FROM assets a JOIN assets m ON m.id = a.group_id
             ORDER BY a.file_path",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        let bundled_previews = sqlx::query_as(
            "SELECT file_path, bundled_preview FROM assets WHERE bundled_preview IS NOT NULL
             ORDER BY file_path",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        DerivedRows {
            folders,
            roles,
            groups,
            bundled_previews,
        }
    }

    /// The rows a fresh library holding `paths` gets from its first pass.
    pub(crate) async fn fresh_pass(paths: &[&str]) -> DerivedRows {
        let pool = memory_library_pool().await;
        for path in paths {
            insert_path(&pool, path).await;
        }
        rebuild(&pool, unmounted()).await.unwrap();
        derived_rows(&pool).await
    }

    /// Indexes `path`, with its id the path itself and its preview done.
    pub(crate) async fn insert_path(pool: &Pool<Sqlite>, path: &str) {
        insert_with_preview(pool, path, "done").await;
    }

    async fn insert_with_preview(pool: &Pool<Sqlite>, path: &str, preview_status: &str) {
        let name = path.rsplit('/').next().unwrap();
        let ext = name.rsplit_once('.').map_or("", |(_, e)| e).to_lowercase();
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at, preview_status)
             VALUES (?, ?, ?, ?, ?, 1, 0, 0, ?)",
        )
        .bind(path)
        .bind(path)
        .bind(name)
        .bind(&ext)
        .bind(AssetKind::from_extension(&ext).as_str())
        .bind(preview_status)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_rerun_with_no_changes_writes_nothing() {
        let pool = memory_library_pool().await;
        for path in KENNEY {
            insert_path(&pool, path).await;
        }
        // 10 folders, the one support file and the two files of a group
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 13);
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 0);
        assert_eq!(derived_rows(&pool).await, fresh_pass(&KENNEY).await);
    }

    #[tokio::test]
    async fn a_removed_file_rewrites_only_its_ancestors_and_the_folders_it_empties() {
        let pool = memory_library_pool().await;
        for path in KENNEY {
            insert_path(&pool, path).await;
        }
        rebuild(&pool, unmounted()).await.unwrap();
        sqlx::query("DELETE FROM assets WHERE file_path = '2D assets/UI Pack/PNG/button.png'")
            .execute(&pool)
            .await
            .unwrap();

        // The root's count changes, and the three folders it emptied go.
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 4);
        let rest: Vec<&str> = KENNEY
            .into_iter()
            .filter(|p| !p.starts_with("2D assets/"))
            .collect();
        assert_eq!(derived_rows(&pool).await, fresh_pass(&rest).await);
    }

    #[tokio::test]
    async fn removing_a_packs_only_marker_unmarks_it_and_shows_its_support_files() {
        let pool = memory_library_pool().await;
        let paths = ["kit/License.txt", "kit/Readme.txt", "kit/rock.glb"];
        for path in paths {
            insert_path(&pool, path).await;
        }
        rebuild(&pool, unmounted()).await.unwrap();
        assert_eq!(
            derived_rows(&pool).await.roles,
            [
                ("kit/License.txt".to_string(), Some("doc".to_string())),
                ("kit/Readme.txt".to_string(), Some("doc".to_string())),
                ("kit/rock.glb".to_string(), None),
            ]
        );
        sqlx::query("DELETE FROM assets WHERE file_path = 'kit/License.txt'")
            .execute(&pool)
            .await
            .unwrap();

        // `kit` and the root change, and the Readme loses its role.
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 3);
        assert_eq!(
            derived_rows(&pool).await,
            fresh_pass(&["kit/Readme.txt", "kit/rock.glb"]).await
        );
    }

    /// Every asset's (path, preview status) and every preview job's (asset, status), by path.
    async fn previews(pool: &Pool<Sqlite>) -> (Vec<(String, String)>, Vec<(String, String)>) {
        let assets =
            sqlx::query_as("SELECT file_path, preview_status FROM assets ORDER BY file_path")
                .fetch_all(pool)
                .await
                .unwrap();
        let jobs =
            sqlx::query_as("SELECT asset_id, status FROM preview_jobs ORDER BY asset_id, status")
                .fetch_all(pool)
                .await
                .unwrap();
        (assets, jobs)
    }

    async fn add_job(pool: &Pool<Sqlite>, asset: &str, status: &str) {
        sqlx::query(
            "INSERT INTO preview_jobs (id, asset_id, status, created_at) VALUES (?, ?, ?, 0)",
        )
        .bind(format!("{asset}/{status}"))
        .bind(asset)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }

    fn pairs(rows: &[(&str, &str)]) -> Vec<(String, String)> {
        rows.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn the_preview_queue_keeps_to_main_items_and_files_on_their_own() {
        let pool = memory_library_pool().await;
        let (glb, fbx, obj, dae) = (
            "kit/GLB format/rock.glb",
            "kit/FBX format/rock.fbx",
            "kit/OBJ format/rock.obj",
            "kit/DAE format/rock.dae",
        );
        // As an index run leaves them: every model waits for a preview, except the OBJ, whose
        // job a worker is running, and the DAE, which already has its thumbnail.
        for path in [glb, fbx, obj] {
            insert_with_preview(&pool, path, "pending").await;
        }
        insert_path(&pool, dae).await;
        add_job(&pool, glb, "pending").await;
        add_job(&pool, fbx, "queued").await;
        add_job(&pool, fbx, "fallback").await;
        add_job(&pool, obj, "processing").await;

        // 6 folders, 4 group ids, and the FBX and the OBJ, which go back to `none`. The FBX
        // loses its waiting jobs, while the OBJ's running job and the DAE's thumbnail stay.
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 12);
        assert_eq!(
            previews(&pool).await,
            (
                pairs(&[
                    (dae, "done"),
                    (fbx, "none"),
                    (glb, "pending"),
                    (obj, "none")
                ]),
                pairs(&[(glb, "pending"), (obj, "processing")]),
            )
        );
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 0);

        // Without the GLB, the FBX leads the group and gets a job of its own.
        sqlx::query("DELETE FROM assets WHERE id = ?")
            .bind(glb)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM preview_jobs WHERE asset_id = ?")
            .bind(glb)
            .execute(&pool)
            .await
            .unwrap();
        rebuild(&pool, unmounted()).await.unwrap();
        assert_eq!(
            previews(&pool).await,
            (
                pairs(&[(dae, "done"), (fbx, "pending"), (obj, "none")]),
                pairs(&[(fbx, "pending"), (obj, "processing")]),
            )
        );
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_group_led_by_a_blend_keeps_its_best_previewable_member_queued() {
        // No glTF or FBX: the `.blend` leads (D12), and no preview reads it
        let paths = [
            "Ships/Bob/Blend/Bob.blend",
            "Ships/Bob/OBJ/Bob.obj",
            "Ships/Bob/DAE/Bob.dae",
        ];
        assert_eq!(groups_of(&paths), [paths.to_vec()]);
        let pool = memory_library_pool().await;
        insert_with_preview(&pool, paths[0], "none").await;
        for path in &paths[1..] {
            insert_with_preview(&pool, path, "pending").await;
            add_job(&pool, path, "pending").await;
        }
        rebuild(&pool, unmounted()).await.unwrap();
        // The OBJ ranks before the DAE, so it stands in, and the DAE comes off the queue
        assert_eq!(
            previews(&pool).await,
            (
                pairs(&[
                    (paths[0], "none"),
                    (paths[2], "none"),
                    (paths[1], "pending")
                ]),
                pairs(&[(paths[1], "pending")]),
            )
        );
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_new_blend_and_obj_pair_queues_the_obj() {
        let pool = memory_library_pool().await;
        let (blend, obj) = ("kit/Blend/rock.blend", "kit/OBJ/rock.obj");
        insert_with_preview(&pool, blend, "none").await;
        insert_with_preview(&pool, obj, "none").await;
        rebuild(&pool, unmounted()).await.unwrap();
        assert_eq!(previews(&pool).await.1, pairs(&[(obj, "pending")]));
    }

    #[tokio::test]
    async fn a_blend_led_group_with_a_render_gets_no_stand_in_until_the_render_goes() {
        let pool = memory_library_pool().await;
        let (blend, obj, render) = (
            "kit/ship.blend",
            "kit/OBJ format/ship.obj",
            "kit/Previews/ship.png",
        );
        for path in [blend, obj, render] {
            insert_with_preview(&pool, path, "none").await;
        }
        rebuild(&pool, unmounted()).await.unwrap();
        // The `.blend` leads and shows the render, so the OBJ stands in for nothing.
        assert_eq!(
            derived_rows(&pool).await.bundled_previews,
            [(blend.to_string(), render.to_string())]
        );
        assert_eq!(
            previews(&pool).await,
            (
                pairs(&[(obj, "none"), (render, "pending"), (blend, "none")]),
                pairs(&[(render, "pending")]),
            )
        );
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 0);

        // Without the render, the OBJ is queued as the group's stand-in.
        sqlx::query("DELETE FROM assets WHERE id = ?")
            .bind(render)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM preview_jobs WHERE asset_id = ?")
            .bind(render)
            .execute(&pool)
            .await
            .unwrap();
        rebuild(&pool, unmounted()).await.unwrap();
        assert_eq!(previews(&pool).await.1, pairs(&[(obj, "pending")]));
    }

    #[tokio::test]
    async fn a_file_on_its_own_needing_no_preview_gets_no_job() {
        let pool = memory_library_pool().await;
        for path in ["kit/rock.blend", "kit/notes.md", "kit/a/tree.glb"] {
            insert_with_preview(&pool, path, "none").await;
        }
        rebuild(&pool, unmounted()).await.unwrap();
        // Only the GLB takes a preview.
        assert_eq!(
            previews(&pool).await.1,
            pairs(&[("kit/a/tree.glb", "pending")])
        );
    }

    #[tokio::test]
    async fn the_preview_queue_skips_members_and_models_with_a_bundled_preview() {
        let pool = memory_library_pool().await;
        let (glb, fbx, render) = (
            "kit/Models/GLB format/sedan.glb",
            "kit/Models/FBX format/sedan.fbx",
            "kit/Previews/sedan.png",
        );
        let (truck_glb, truck_fbx) = (
            "kit/Models/GLB format/truck.glb",
            "kit/Models/FBX format/truck.fbx",
        );
        // As a first index leaves them: nothing queued yet, except the sedan's GLB, which an
        // older build queued.
        for path in [glb, fbx, render, truck_glb, truck_fbx] {
            insert_with_preview(&pool, path, "none").await;
        }
        sqlx::query("UPDATE assets SET preview_status = 'pending' WHERE id = ?")
            .bind(glb)
            .execute(&pool)
            .await
            .unwrap();
        add_job(&pool, glb, "pending").await;
        rebuild(&pool, unmounted()).await.unwrap();
        // The sedan shows its render, so neither of its files is queued, and the render is, for
        // its card. The truck has no render, so its GLB is queued and its FBX isn't.
        assert_eq!(
            previews(&pool).await,
            (
                pairs(&[
                    (fbx, "none"),
                    (truck_fbx, "none"),
                    (glb, "none"),
                    (truck_glb, "pending"),
                    (render, "pending"),
                ]),
                pairs(&[(truck_glb, "pending"), (render, "pending")]),
            )
        );
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 0);

        // Without its GLB, the truck's FBX leads with no picture and gets a job. Without its
        // render, the sedan's GLB does.
        for path in [truck_glb, render] {
            sqlx::query("DELETE FROM assets WHERE id = ?")
                .bind(path)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("DELETE FROM preview_jobs WHERE asset_id = ?")
                .bind(path)
                .execute(&pool)
                .await
                .unwrap();
        }
        rebuild(&pool, unmounted()).await.unwrap();
        assert_eq!(
            previews(&pool).await,
            (
                pairs(&[(fbx, "none"), (truck_fbx, "pending"), (glb, "pending")]),
                pairs(&[(truck_fbx, "pending"), (glb, "pending")]),
            )
        );
        assert_eq!(derived_rows(&pool).await.bundled_previews, []);
    }

    #[tokio::test]
    async fn a_stack_is_one_group_whose_main_item_alone_stays_queued() {
        let (default, double, vector) = (
            "UI Pack/PNG/Blue/Default/button.png",
            "UI Pack/PNG/Blue/Double/button.png",
            "UI Pack/Vector/Blue/button.svg",
        );
        let pool = memory_library_pool().await;
        for path in [default, double, vector] {
            insert_with_preview(&pool, path, "pending").await;
            add_job(&pool, path, "pending").await;
        }
        rebuild(&pool, unmounted()).await.unwrap();
        let rows = derived_rows(&pool).await;
        let main = Some(default.to_string());
        assert_eq!(
            rows.groups,
            [default, double, vector].map(|path| (path.to_string(), main.clone()))
        );
        assert_eq!(rows, fresh_pass(&[default, double, vector]).await);
        assert_eq!(
            previews(&pool).await,
            (
                pairs(&[(default, "pending"), (double, "none"), (vector, "none")]),
                pairs(&[(default, "pending")]),
            )
        );
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_model_whose_only_render_the_webview_cant_display_keeps_its_preview_job() {
        let pool = memory_library_pool().await;
        let (glb, render) = ("kit/rock.glb", "kit/Renders/rock.dds");
        for path in [glb, render] {
            insert_with_preview(&pool, path, "none").await;
        }
        rebuild(&pool, unmounted()).await.unwrap();
        assert_eq!(derived_rows(&pool).await.bundled_previews, []);
        // The DDS takes no preview, so only the model is queued.
        assert_eq!(previews(&pool).await.1, pairs(&[(glb, "pending")]));
    }

    #[tokio::test]
    async fn a_render_is_recorded_once_and_the_rerun_writes_nothing() {
        let paths = [
            "kit/Models/GLB format/sedan.glb",
            "kit/Models/OBJ format/sedan.obj",
            "kit/Previews/sedan.png",
        ];
        let pool = memory_library_pool().await;
        for path in paths {
            insert_path(&pool, path).await;
        }
        // 6 folders, the render's role, 2 group ids and the GLB's bundled preview
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 10);
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 0);
        assert_eq!(
            derived_rows(&pool).await.bundled_previews,
            [(paths[0].to_string(), paths[2].to_string())]
        );
    }

    /// The pack info `pool` holds for folder `path`.
    async fn stored_pack(pool: &Pool<Sqlite>, path: &str) -> PackInfo {
        let (json,): (Option<String>,) =
            sqlx::query_as("SELECT pack_json FROM folders WHERE path = ?")
                .bind(path)
                .fetch_one(pool)
                .await
                .unwrap();
        parse_pack(json.as_deref()).expect("pack_json holds pack info")
    }

    #[tokio::test]
    async fn the_pass_reads_a_packs_license_and_rereads_it_only_once_it_changes() {
        let root = tempfile::tempdir().unwrap();
        let license = root.path().join("kit/License.txt");
        std::fs::create_dir_all(license.parent().unwrap()).unwrap();
        std::fs::write(&license, KENNEY_LICENSE).unwrap();
        let pool = memory_library_pool().await;
        for path in ["kit/License.txt", "kit/Preview.png", "kit/Models/sedan.glb"] {
            insert_path(&pool, path).await;
        }

        // Unreadable, the license names nothing and records no read.
        rebuild(&pool, unmounted()).await.unwrap();
        let unread = stored_pack(&pool, "kit").await;
        assert_eq!(unread.name, "kit");
        assert_eq!(unread.cover.as_deref(), Some("kit/Preview.png"));
        assert_eq!(unread.license.as_ref().map(|l| l.size_bytes), Some(0));

        rebuild(&pool, root.path()).await.unwrap();
        let read = stored_pack(&pool, "kit").await;
        assert_eq!(read.name, "Car Kit");
        let file = read.license.unwrap();
        assert_eq!(file.text, parse_license(KENNEY_LICENSE));
        assert_eq!((file.parser, file.size_bytes), (LICENSE_PARSER, 1));

        // An unchanged row means an unchanged file: the pass doesn't read it again, even when
        // the file can't be read at all.
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 0);
        assert_eq!(
            stored_pack(&pool, "kit").await.license.unwrap().text,
            file.text
        );
        std::fs::write(
            &license,
            "Space Kit (1.2)\nby Kenney Vleugels (Kenney.nl)\n",
        )
        .unwrap();
        assert_eq!(rebuild(&pool, root.path()).await.unwrap(), 0);
        assert_eq!(stored_pack(&pool, "kit").await.name, "Car Kit");
        sqlx::query("UPDATE assets SET modified_at = 5 WHERE file_path = 'kit/License.txt'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(rebuild(&pool, root.path()).await.unwrap(), 1);
        assert_eq!(stored_pack(&pool, "kit").await.name, "Space Kit");
        assert_eq!(rebuild(&pool, root.path()).await.unwrap(), 0);

        // Changed again but unreadable, as on a share that dropped: the pack keeps its name, and
        // records no read, so the next pass reads the file once it can.
        sqlx::query("UPDATE assets SET modified_at = 6 WHERE file_path = 'kit/License.txt'")
            .execute(&pool)
            .await
            .unwrap();
        rebuild(&pool, unmounted()).await.unwrap();
        let dropped = stored_pack(&pool, "kit").await;
        assert_eq!(dropped.name, "Space Kit");
        let file = dropped.license.unwrap();
        assert_eq!(
            (
                file.text.header.map(|h| h.name),
                file.parser,
                file.size_bytes,
                file.modified_at
            ),
            (Some("Space Kit".to_string()), 0, 0, 0)
        );
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 0);
        std::fs::write(&license, "Car Kit (3.2)\nby Kenney Vleugels (Kenney.nl)\n").unwrap();
        assert_eq!(rebuild(&pool, root.path()).await.unwrap(), 1);
        let back = stored_pack(&pool, "kit").await;
        assert_eq!(back.name, "Car Kit");
        assert_eq!(back.license.map(|l| l.modified_at), Some(6));
    }

    #[tokio::test]
    async fn a_license_read_by_an_older_parser_is_read_again_once() {
        let root = tempfile::tempdir().unwrap();
        let license = root.path().join("kit/License.txt");
        std::fs::create_dir_all(license.parent().unwrap()).unwrap();
        std::fs::write(&license, KENNEY_LICENSE).unwrap();
        let pool = memory_library_pool().await;
        insert_path(&pool, "kit/License.txt").await;
        rebuild(&pool, root.path()).await.unwrap();
        // The pack info a pass before `LicenseFile::parser` wrote, from the same file
        sqlx::query(
            r#"UPDATE folders SET pack_json = '{"name":"Car Kit","cover":null,"license":{"path":"kit/License.txt","headerName":"Car Kit","sizeBytes":1,"modifiedAt":0}}' WHERE path = 'kit'"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            stored_pack(&pool, "kit").await.license.unwrap().text,
            LicenseText::default()
        );

        assert_eq!(rebuild(&pool, root.path()).await.unwrap(), 1);
        let file = stored_pack(&pool, "kit").await.license.unwrap();
        assert_eq!(
            (file.text, file.parser),
            (parse_license(KENNEY_LICENSE), LICENSE_PARSER)
        );
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_count_change_keeps_the_pack_info_and_writes_only_the_counts() {
        let pool = memory_library_pool().await;
        for path in KENNEY {
            insert_path(&pool, path).await;
        }
        rebuild(&pool, unmounted()).await.unwrap();
        let crate_glb = "3D assets/Platformer Kit/Models/crate.glb";
        insert_path(&pool, crate_glb).await;
        // The kit, `Models`, `3D assets` and the root
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 4);
        let paths = [&KENNEY[..], &[crate_glb]].concat();
        assert_eq!(
            stored_pack(&pool, "3D assets/Platformer Kit").await,
            info(&derive(paths), "3D assets/Platformer Kit").clone()
        );
    }

    #[tokio::test]
    async fn a_commands_pass_waits_for_the_index_run_holding_the_lock() {
        let pool = memory_library_pool().await;
        let library = crate::db::memory_library("lib", pool.clone());
        let db = std::sync::Arc::new(crate::db::memory_database(vec![library.clone()]).await);
        let lock = db.index_lock("lib");
        let run = lock.lock().await;

        let pass = tokio::spawn({
            let db = db.clone();
            async move { rebuild_library(&db, &library).await }
        });
        insert_path(&pool, "kits/kit/model.glb").await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !pass.is_finished(),
            "the pass ran while an index run held the lock"
        );
        assert!(derived_rows(&pool).await.folders.is_empty());

        drop(run);
        pass.await.unwrap();
        assert_eq!(
            derived_rows(&pool).await,
            fresh_pass(&["kits/kit/model.glb"]).await
        );
    }

    #[tokio::test]
    async fn a_pass_of_more_than_one_batch_writes_every_row() {
        let pool = memory_library_pool().await;
        let paths: Vec<String> = (0..450)
            .flat_map(|i| {
                [
                    format!("kits/kit {i}/model.glb"),
                    format!("kits/kit {i}/License.txt"),
                ]
            })
            .collect();
        for path in &paths {
            insert_path(&pool, path).await;
        }
        // The root, `kits`, 450 kits and 450 licenses
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 902);
        sqlx::query("DELETE FROM assets WHERE file_path LIKE 'kits/kit 1%'")
            .execute(&pool)
            .await
            .unwrap();
        let rest: Vec<&str> = paths
            .iter()
            .map(String::as_str)
            .filter(|p| !p.starts_with("kits/kit 1"))
            .collect();
        // 111 kits go (1, 10-19, 100-199), and the root and `kits` change their count.
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 113);
        assert_eq!(derived_rows(&pool).await, fresh_pass(&rest).await);
    }

    /// The Car Kit's sedan in its three formats, by path. The GLB is the main item.
    pub(crate) const SEDAN: [&str; 3] = [
        "3D assets/Car Kit/Models/FBX format/sedan.fbx",
        "3D assets/Car Kit/Models/GLB format/sedan.glb",
        "3D assets/Car Kit/Models/OBJ format/sedan.obj",
    ];
    /// Kenney's render of the sedan, which is no member of its group.
    pub(crate) const SEDAN_RENDER: &str = "3D assets/Car Kit/Previews/sedan.png";

    /// A library holding `KENNEY_3D_KIT` after its first pass, each asset's id its path.
    pub(crate) async fn car_kit() -> Pool<Sqlite> {
        let pool = memory_library_pool().await;
        for path in KENNEY_3D_KIT {
            insert_path(&pool, path).await;
        }
        rebuild(&pool, unmounted()).await.unwrap();
        pool
    }

    /// Every file holding a row of `sql` (`SELECT asset_id, <value> FROM ...`), by path, with
    /// that row's value.
    pub(crate) async fn rows(pool: &Pool<Sqlite>, sql: &str) -> Vec<(String, String)> {
        sqlx::query_as(&format!("SELECT * FROM ({sql}) ORDER BY 1, 2"))
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// `(id, value)` for each of `ids`, in order.
    pub(crate) fn each(ids: &[&str], value: &str) -> Vec<(String, String)> {
        ids.iter()
            .map(|id| (id.to_string(), value.to_string()))
            .collect()
    }

    async fn run(pool: &Pool<Sqlite>, sql: &str) {
        sqlx::query(sql).execute(pool).await.unwrap();
    }

    #[tokio::test]
    async fn group_members_are_a_files_whole_group_or_the_file_alone() {
        let pool = car_kit().await;
        for id in SEDAN {
            let mut members = group_members(&pool, id).await.unwrap();
            members.sort();
            assert_eq!(members, SEDAN, "{id}");
        }
        for id in [SEDAN_RENDER, "3D assets/Car Kit/Preview.png", "not indexed"] {
            assert_eq!(group_members(&pool, id).await.unwrap(), [id]);
        }
    }

    /// The metadata columns of every model of the sedan and of its render.
    async fn sedan_fields(pool: &Pool<Sqlite>) -> Vec<(String, String)> {
        rows(
            pool,
            "SELECT id, rating || ' ' || color_label || ' ' || is_favorite || ' ' || description
             FROM assets WHERE file_path LIKE '%/sedan.%'",
        )
        .await
    }

    #[tokio::test]
    async fn a_group_that_forms_gives_every_member_the_union_of_its_files_metadata() {
        let pool = memory_library_pool().await;
        for path in [SEDAN[0], SEDAN_RENDER] {
            insert_path(&pool, path).await;
        }
        rebuild(&pool, unmounted()).await.unwrap();
        // The FBX on its own is tagged, collected, rated and described, and so is the render.
        for sql in [
            "INSERT INTO asset_tags (asset_id, tag_id) VALUES
                 ('3D assets/Car Kit/Models/FBX format/sedan.fbx', 'car'),
                 ('3D assets/Car Kit/Previews/sedan.png', 'render')",
            "INSERT INTO collection_assets (collection_id, asset_id) VALUES
                 ('vehicles', '3D assets/Car Kit/Models/FBX format/sedan.fbx'),
                 ('pictures', '3D assets/Car Kit/Previews/sedan.png')",
            "UPDATE assets SET rating = 4, color_label = 'blue', description = 'four doors'",
        ] {
            run(&pool, sql).await;
        }
        // The GLB and OBJ arrive, the GLB with a tag and a color label of its own.
        for path in [SEDAN[1], SEDAN[2]] {
            insert_path(&pool, path).await;
        }
        run(
            &pool,
            "INSERT INTO asset_tags (asset_id, tag_id) VALUES
                 ('3D assets/Car Kit/Models/GLB format/sedan.glb', 'low-poly');
             UPDATE assets SET color_label = 'red' WHERE extension = 'glb'",
        )
        .await;

        rebuild(&pool, unmounted()).await.unwrap();

        let mut tags = [each(&SEDAN, "car"), each(&SEDAN, "low-poly")].concat();
        tags.push((SEDAN_RENDER.into(), "render".into()));
        tags.sort();
        assert_eq!(
            rows(&pool, "SELECT asset_id, tag_id FROM asset_tags").await,
            tags
        );
        let mut collections = each(&SEDAN, "vehicles");
        collections.push((SEDAN_RENDER.into(), "pictures".into()));
        assert_eq!(
            rows(
                &pool,
                "SELECT asset_id, collection_id FROM collection_assets"
            )
            .await,
            collections
        );
        // The GLB's color label wins. It has no rating or description, so the FBX's fill in.
        let mut fields = each(&SEDAN, "4 red 0 four doors");
        fields.push((SEDAN_RENDER.into(), "4 blue 0 four doors".into()));
        assert_eq!(sedan_fields(&pool).await, fields);
    }

    #[tokio::test]
    async fn a_file_that_joins_a_group_gets_its_tags_and_its_main_items_fields() {
        let pool = memory_library_pool().await;
        for path in [SEDAN[0], SEDAN[1]] {
            insert_path(&pool, path).await;
        }
        rebuild(&pool, unmounted()).await.unwrap();
        run(
            &pool,
            "INSERT INTO asset_tags (asset_id, tag_id) VALUES
                 ('3D assets/Car Kit/Models/FBX format/sedan.fbx', 'car'),
                 ('3D assets/Car Kit/Models/GLB format/sedan.glb', 'car');
             UPDATE assets SET rating = 2, is_favorite = 1, description = 'sedan'
                 WHERE extension = 'glb';
             UPDATE assets SET rating = 5, color_label = 'green', description = 'old'
                 WHERE extension = 'fbx'",
        )
        .await;
        insert_path(&pool, SEDAN[2]).await;
        run(
            &pool,
            "INSERT INTO asset_tags (asset_id, tag_id) VALUES
                 ('3D assets/Car Kit/Models/OBJ format/sedan.obj', 'wavefront');
             UPDATE assets SET rating = 1 WHERE extension = 'obj'",
        )
        .await;

        rebuild(&pool, unmounted()).await.unwrap();

        assert_eq!(
            rows(&pool, "SELECT asset_id, tag_id FROM asset_tags").await,
            {
                let mut tags = [each(&SEDAN, "car"), each(&SEDAN, "wavefront")].concat();
                tags.sort();
                tags
            }
        );
        // The GLB's rating, favorite and description; the FBX's color label, the GLB having none.
        assert_eq!(sedan_fields(&pool).await, each(&SEDAN, "2 green 1 sedan"));
    }

    #[tokio::test]
    async fn a_dissolved_groups_files_keep_what_they_have() {
        let pool = car_kit().await;
        run(
            &pool,
            "INSERT INTO asset_tags (asset_id, tag_id) SELECT id, 'car' FROM assets
                 WHERE file_path LIKE '%/sedan.%' AND extension != 'png';
             UPDATE assets SET rating = 3;
             DELETE FROM assets WHERE file_path LIKE '%/sedan.glb' OR file_path LIKE '%/sedan.obj'",
        )
        .await;

        rebuild(&pool, unmounted()).await.unwrap();

        // The FBX is on its own now, with the tag and the rating it had.
        assert_eq!(
            rows(
                &pool,
                "SELECT asset_id, tag_id FROM asset_tags WHERE asset_id IN (SELECT id FROM assets)"
            )
            .await,
            each(&SEDAN[..1], "car")
        );
        assert_eq!(
            sedan_fields(&pool).await,
            [
                (SEDAN[0].to_string(), "3 none 0 ".to_string()),
                (SEDAN_RENDER.to_string(), "3 none 0 ".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn groups_formed_before_group_writes_union_once() {
        let pool = car_kit().await;
        run(
            &pool,
            "DELETE FROM library_meta;
             INSERT INTO asset_tags (asset_id, tag_id) VALUES
                 ('3D assets/Car Kit/Models/FBX format/sedan.fbx', 'car')",
        )
        .await;

        rebuild(&pool, unmounted()).await.unwrap();
        assert_eq!(
            rows(&pool, "SELECT asset_id, tag_id FROM asset_tags").await,
            each(&SEDAN, "car")
        );

        // Once recorded, a pass with no change to the group leaves the tags alone.
        run(&pool, "DELETE FROM asset_tags WHERE asset_id LIKE '%.obj'").await;
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 0);
        assert_eq!(
            rows(&pool, "SELECT asset_id, tag_id FROM asset_tags").await,
            each(&SEDAN[..2], "car")
        );
    }

    /// Statements that write one asset's metadata.
    const METADATA_WRITES: [&str; 8] = [
        "UPDATE assets SET rating",
        "UPDATE assets SET color_label",
        "UPDATE assets SET is_favorite",
        "UPDATE assets SET description",
        "INTO asset_tags",
        "INTO collection_assets",
        "DELETE FROM asset_tags WHERE asset_id = ? AND",
        "DELETE FROM collection_assets WHERE collection_id = ? AND asset_id",
    ];

    /// Every function outside tests that holds one of `METADATA_WRITES`, by file, and whether it
    /// calls `group_members`.
    fn metadata_writers() -> Vec<(String, String, bool)> {
        fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    sources(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        sources(&root, &mut files);
        files.sort();
        let mut writers = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            let code = text.split("mod tests {").next().unwrap();
            // Each top-level item, from one unindented line to the next.
            let mut items: Vec<String> = Vec::new();
            for line in code.lines() {
                if items.is_empty() || !(line.is_empty() || line.starts_with([' ', '}', ')'])) {
                    items.push(String::new());
                }
                let item = items.last_mut().unwrap();
                item.push_str(line);
                item.push('\n');
            }
            for item in items {
                if !METADATA_WRITES.iter().any(|w| item.contains(w)) {
                    continue;
                }
                let name = item
                    .split_once("fn ")
                    .map(|(_, rest)| rest.split(['(', '<']).next().unwrap().to_string())
                    .unwrap_or_default();
                let file = file.strip_prefix(&root).unwrap().display().to_string();
                writers.push((file, name, item.contains("group_members(")));
            }
        }
        writers
    }

    #[test]
    fn no_metadata_write_bypasses_group_members() {
        let writers = metadata_writers();
        let names: Vec<(&str, &str, bool)> = writers
            .iter()
            .map(|(file, name, expands)| (file.as_str(), name.as_str(), *expands))
            .collect();
        assert_eq!(
            names,
            [
                ("autotag/store.rs", "write_batch", true),
                ("autotag/undo.rs", "remove_pairs", true),
                ("commands/asset.rs", "apply_asset_patch", true),
                ("commands/collection.rs", "add_assets_to_collection", true),
                (
                    "commands/collection.rs",
                    "remove_assets_from_collection",
                    true
                ),
                // The union itself, which writes to a group's members by their ids.
                ("structure.rs", "apply_unions", false),
            ],
            "a metadata write must expand its asset through `group_members` (plan C20)"
        );
    }
}
