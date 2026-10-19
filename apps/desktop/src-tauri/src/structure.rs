//! The structure pass: a library's folder tree, its packs, its support files and its format
//! groups, derived from the paths it has indexed (docs/plans/smarter-big-collections/plan.md,
//! "The structure pass"). `derive` is a pure function over the paths, and the pass writes only
//! the folder rows and `assets.support_role` and `assets.group_id` values that differ from it.
//! Every index run ends with the pass (`watcher::index_paths`), and so does every command that
//! changes the paths, through `rebuild_library`. Nothing else writes `folders`, `support_role`
//! or `group_id`.

use sqlx::{Pool, Sqlite};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Bound;
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
const DOC_EXTENSIONS: [&str; 3] = ["txt", "md", "pdf"];
/// The formats a group picks its main item by (plan D12), best first. Every other format ranks
/// after them.
const MAIN_FORMATS: [&str; 5] = ["glb", "gltf", "fbx", "blend", "obj"];
/// The formats a folder of model copies is named after, ignoring case and an optional ` format`
/// or ` files` (`GLB format`, `FBX`, `obj files`). `fbx(unity)` and `fbx (unity)` are FBX
/// folders too.
const FORMAT_FOLDERS: [&str; 11] = [
    "glb", "gltf", "fbx", "obj", "blend", "dae", "stl", "3ds", "usd", "usdz", "ply",
];

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
}

/// Why a file is a pack's support file, hidden from the grid unless "Show hidden files" is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportRole {
    /// A doc (`txt`, `md`, `pdf`): a license, a readme
    Doc,
    /// A `Preview*` or `Sample*` image
    Cover,
}

impl SupportRole {
    pub fn as_str(self) -> &'static str {
        match self {
            SupportRole::Doc => "doc",
            SupportRole::Cover => "cover",
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
/// 5. **Counts:** each folder counts the grid items with a visible file below it, a group once,
///    and the support files below it.
pub fn derive<'a>(paths: impl IntoIterator<Item = &'a str>) -> Structure {
    let paths: Vec<&str> = paths.into_iter().collect();
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

    let roles: Vec<Option<SupportRole>> = paths
        .iter()
        .zip(&kinds)
        .map(|(path, kind)| {
            let (dir, _) = split_dir(path);
            (packs.contains(dir) || (marked.contains(dir) && in_pack(&packs, dir)))
                .then(|| kind.support_role())
                .flatten()
        })
        .collect();
    let groups = format_groups(&paths, &kinds, &roles);

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
    for pack in packs {
        folders.get_mut(pack).expect("a pack has a row").is_pack = true;
    }
    Structure {
        folders,
        roles,
        groups,
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
    for files in candidates.into_values().filter(|f| f.len() > 1) {
        let (_, main) = *files
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
        for (_, i) in files {
            groups[i] = Some(main);
        }
    }
    groups
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
    preview_status: String,
}

pub(crate) type FolderRow = (String, Option<String>, String, bool, i64, i64, i64);

/// Makes `folders` hold exactly `folders`, writing only the rows that differ, in transactions of
/// `BATCH` rows. Returns how many rows it inserted, updated or deleted.
async fn apply_folders(
    pool: &Pool<Sqlite>,
    folders: &BTreeMap<String, Folder>,
) -> sqlx::Result<usize> {
    let rows: Vec<FolderRow> = sqlx::query_as(
        "SELECT path, parent_path, name, is_pack, item_count, hidden_count, split_groups
         FROM folders",
    )
    .fetch_all(pool)
    .await?;
    let current: HashMap<String, Folder> = rows
        .into_iter()
        .map(
            |(path, parent_path, name, is_pack, item_count, hidden_count, split_groups)| {
                let folder = Folder {
                    parent_path,
                    name,
                    is_pack,
                    item_count,
                    hidden_count,
                    split_groups,
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
            // An upsert, so a later step's pack info survives a change in the counts.
            sqlx::query(
                "INSERT INTO folders
                     (path, parent_path, name, is_pack, item_count, hidden_count, split_groups)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(path) DO UPDATE SET parent_path = excluded.parent_path,
                     name = excluded.name, is_pack = excluded.is_pack,
                     item_count = excluded.item_count, hidden_count = excluded.hidden_count,
                     split_groups = excluded.split_groups",
            )
            .bind(path)
            .bind(&folder.parent_path)
            .bind(&folder.name)
            .bind(folder.is_pack)
            .bind(folder.item_count)
            .bind(folder.hidden_count)
            .bind(folder.split_groups)
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

/// Keeps group members off the preview queue (plan C15). A group member other than its main item
/// comes off the queue, keeping any thumbnail it has. Any other file at `none` whose format takes
/// a preview goes on it: a new file, a member that became a main item, or a pack's cover image,
/// which "Show hidden files" shows and a pack card pictures. `preview::unqueue` and
/// `preview::enqueue` own how. Returns how many assets it changed.
async fn apply_previews(
    pool: &Pool<Sqlite>,
    files: &[FileRow],
    structure: &Structure,
) -> sqlx::Result<usize> {
    let waiting = crate::preview::assets_waiting_for_jobs(pool).await?;
    let mut members = Vec::new();
    let mut items = Vec::new();
    for (i, file) in files.iter().enumerate() {
        let main = structure.groups[i];
        if main.is_some_and(|m| m != i) {
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

/// Runs the pass on the library DB at `pool`: reads every indexed path, derives the structure and
/// applies it. Returns the rows written. The caller holds the library's `index_lock`.
pub async fn rebuild(pool: &Pool<Sqlite>) -> sqlx::Result<usize> {
    let started = Instant::now();
    let files: Vec<FileRow> = sqlx::query_as(
        "SELECT id, file_path, extension, support_role, group_id, preview_status FROM assets",
    )
    .fetch_all(pool)
    .await?;
    let structure = derive(files.iter().map(|f| f.file_path.as_str()));
    let written = apply_folders(pool, &structure.folders).await?
        + apply_roles(pool, &files, &structure.roles).await?
        + apply_groups(pool, &files, &structure.groups).await?
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
        "Structure pass: {} files, {} folders, {} packs, {} support files, {} groups of {} files, {} rows written in {:?}",
        files.len(),
        structure.folders.len(),
        structure.folders.values().filter(|f| f.is_pack).count(),
        structure.roles.iter().flatten().count(),
        mains,
        grouped,
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
    if let Err(e) = rebuild(&library.pool).await {
        tracing::error!(target: "structure", "Structure pass failed for library {}: {e}", library.id);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::memory_library_pool;

    fn folder(parent_path: Option<&str>, name: &str, item_count: i64, hidden_count: i64) -> Folder {
        Folder {
            parent_path: parent_path.map(str::to_string),
            name: name.to_string(),
            is_pack: false,
            item_count,
            hidden_count,
            split_groups: 0,
        }
    }

    fn split(folder: Folder, split_groups: i64) -> Folder {
        Folder {
            split_groups,
            ..folder
        }
    }

    fn pack(folder: Folder) -> Folder {
        Folder {
            is_pack: true,
            ..folder
        }
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
                    pack(folder(Some("3D assets"), "Platformer Kit", 1, 1)),
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
        assert_eq!(
            derive(reversed),
            Structure {
                roles,
                groups,
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
    fn a_kenney_3d_kit_is_a_pack_whose_docs_and_covers_are_hidden() {
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
                ("3D assets/Holiday Kit/License.txt", "doc"),
            ]
        );
        // The sedan in three formats is one item, next to the texture and the render.
        let kit = &structure.folders["3D assets/Car Kit"];
        assert_eq!((kit.item_count, kit.hidden_count), (3, 3));
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
        // model and the texture once. The render outside the format folders stays its own item.
        let structure = derive(paths);
        let count = |path: &str| structure.folders[path].item_count;
        assert_eq!(count("3D assets/Car Kit/Models/GLB format"), 3);
        assert_eq!(count("3D assets/Car Kit/Models/OBJ format"), 3);
        assert_eq!(count("3D assets/Car Kit/Models/FBX format"), 2);
        assert_eq!(count("3D assets/Car Kit/Models/FBX format/Textures"), 1);
        assert_eq!(count("3D assets/Car Kit/Models"), 3);
        assert_eq!(count("3D assets/Car Kit"), 4);
        assert_eq!(count(""), 4);
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

    /// Every `folders` row, every asset's support role, and the main item's path of every
    /// grouped asset, in path order.
    #[derive(Debug, PartialEq)]
    pub(crate) struct DerivedRows {
        pub folders: Vec<FolderRow>,
        pub roles: Vec<(String, Option<String>)>,
        pub groups: Vec<(String, Option<String>)>,
    }

    /// What the pass has written to `pool`, for comparing a library with a fresh pass.
    pub(crate) async fn derived_rows(pool: &Pool<Sqlite>) -> DerivedRows {
        let folders = sqlx::query_as(
            "SELECT path, parent_path, name, is_pack, item_count, hidden_count, split_groups
             FROM folders ORDER BY path",
        )
        .fetch_all(pool)
        .await
        .unwrap();
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
        DerivedRows {
            folders,
            roles,
            groups,
        }
    }

    /// The rows a fresh library holding `paths` gets from its first pass.
    pub(crate) async fn fresh_pass(paths: &[&str]) -> DerivedRows {
        let pool = memory_library_pool().await;
        for path in paths {
            insert_path(&pool, path).await;
        }
        rebuild(&pool).await.unwrap();
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
        assert_eq!(rebuild(&pool).await.unwrap(), 13);
        assert_eq!(rebuild(&pool).await.unwrap(), 0);
        assert_eq!(derived_rows(&pool).await, fresh_pass(&KENNEY).await);
    }

    #[tokio::test]
    async fn a_removed_file_rewrites_only_its_ancestors_and_the_folders_it_empties() {
        let pool = memory_library_pool().await;
        for path in KENNEY {
            insert_path(&pool, path).await;
        }
        rebuild(&pool).await.unwrap();
        sqlx::query("DELETE FROM assets WHERE file_path = '2D assets/UI Pack/PNG/button.png'")
            .execute(&pool)
            .await
            .unwrap();

        // The root's count changes, and the three folders it emptied go.
        assert_eq!(rebuild(&pool).await.unwrap(), 4);
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
        rebuild(&pool).await.unwrap();
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
        assert_eq!(rebuild(&pool).await.unwrap(), 3);
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
        assert_eq!(rebuild(&pool).await.unwrap(), 12);
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
        assert_eq!(rebuild(&pool).await.unwrap(), 0);

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
        rebuild(&pool).await.unwrap();
        assert_eq!(
            previews(&pool).await,
            (
                pairs(&[(dae, "done"), (fbx, "pending"), (obj, "none")]),
                pairs(&[(fbx, "pending"), (obj, "processing")]),
            )
        );
        assert_eq!(rebuild(&pool).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_file_on_its_own_needing_no_preview_gets_no_job() {
        let pool = memory_library_pool().await;
        for path in ["kit/rock.blend", "kit/notes.md", "kit/a/tree.glb"] {
            insert_with_preview(&pool, path, "none").await;
        }
        rebuild(&pool).await.unwrap();
        // Only the GLB takes a preview.
        assert_eq!(
            previews(&pool).await.1,
            pairs(&[("kit/a/tree.glb", "pending")])
        );
    }

    #[tokio::test]
    async fn a_count_change_keeps_the_pack_info() {
        let pool = memory_library_pool().await;
        for path in KENNEY {
            insert_path(&pool, path).await;
        }
        rebuild(&pool).await.unwrap();
        sqlx::query("UPDATE folders SET pack_json = '{}' WHERE path = '3D assets/Platformer Kit'")
            .execute(&pool)
            .await
            .unwrap();
        insert_path(&pool, "3D assets/Platformer Kit/Preview.png").await;
        rebuild(&pool).await.unwrap();

        let row: (Option<String>, i64, i64) = sqlx::query_as(
            "SELECT pack_json, item_count, hidden_count FROM folders
             WHERE path = '3D assets/Platformer Kit'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row, (Some("{}".to_string()), 1, 2));
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
        assert_eq!(rebuild(&pool).await.unwrap(), 902);
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
        assert_eq!(rebuild(&pool).await.unwrap(), 113);
        assert_eq!(derived_rows(&pool).await, fresh_pass(&rest).await);
    }
}
