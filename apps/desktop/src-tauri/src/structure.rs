//! The structure pass: a library's folder tree, its packs and its support files, derived from the
//! paths it has indexed (docs/plans/smarter-big-collections/plan.md, "The structure pass").
//! `derive` is a pure function over the paths and the license headers, and the pass writes only
//! the folder rows and `assets.support_role` values that differ from it. Every index run ends with
//! the pass (`watcher::index_paths`), and so does every command that changes the paths, through
//! `rebuild_library`. Nothing else writes `folders` or `support_role`.

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
/// How much of a license file the pass reads for its header.
const HEADER_BYTES: u64 = 4096;

/// One folder as the pass derives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// `None` for the root
    pub parent_path: Option<String>,
    pub name: String,
    pub is_pack: bool,
    /// Visible files below the folder, at any depth: every file but support files.
    pub item_count: i64,
    /// Support files below the folder, at any depth
    pub hidden_count: i64,
    /// A pack's info, stored as `folders.pack_json`; `None` for a folder that is no pack
    pub pack: Option<PackInfo>,
}

/// What the pass derives for one pack (plan C9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackInfo {
    /// The name shown: the license header's, else the folder's
    pub name: String,
    /// The cover's path: the first `Preview*` cover image, else the first `Sample*` one, else the
    /// pack's first image, else its first visible file (a model, whose thumbnail shows), each by
    /// path
    pub cover: Option<String>,
    pub license: Option<LicenseFile>,
}

/// A pack's license file: its first `License*` doc by path, directly inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseFile {
    pub path: String,
    /// The pack's name from the file's Kenney header, if it has one (`kenney_header_name`)
    pub header_name: Option<String>,
    /// The file's size and mtime when the pass read its header, so a rerun reads only a changed
    /// file. `derive` leaves them 0, and `rebuild` fills them from the file's row.
    #[serde(default)]
    pub size_bytes: i64,
    #[serde(default)]
    pub modified_at: i64,
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

/// A file's lowercase stem, and whether it is a doc or an image.
struct FileKind {
    stem: String,
    is_doc: bool,
    is_image: bool,
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
/// 4. **Counts:** each folder counts the visible and the support files below it.
/// 5. **Pack info** (C9): each pack's cover and license file, and its name, which is the license
///    file's header name (`header_name` reads it) or else the folder's.
pub fn derive<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    mut header_name: impl FnMut(&str) -> Option<String>,
) -> Structure {
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

    let mut roles = Vec::with_capacity(paths.len());
    for (path, kind) in paths.iter().zip(&kinds) {
        let (dir, _) = split_dir(path);
        let role = (packs.contains(dir) || (marked.contains(dir) && in_pack(&packs, dir)))
            .then(|| kind.support_role())
            .flatten();
        roles.push(role);
        for dir in ancestors(path) {
            let folder = folders.get_mut(dir).expect("every ancestor has a row");
            if role.is_some() {
                folder.hidden_count += 1;
            } else {
                folder.item_count += 1;
            }
        }
    }
    let mut info = pack_files(&packs, &paths, &kinds, &roles);
    for pack in packs {
        let folder = folders.get_mut(pack).expect("a pack has a row");
        let files = info.remove(pack).unwrap_or_default();
        let license = files.license.map(|path| LicenseFile {
            header_name: header_name(path),
            path: path.to_string(),
            size_bytes: 0,
            modified_at: 0,
        });
        folder.is_pack = true;
        folder.pack = Some(PackInfo {
            name: license
                .as_ref()
                .and_then(|l| l.header_name.clone())
                .unwrap_or_else(|| folder.name.clone()),
            cover: files.cover().map(str::to_string),
            license,
        });
    }
    Structure { folders, roles }
}

/// The files of one pack that its info is chosen from, each the first by path.
#[derive(Default)]
struct PackFiles<'a> {
    preview: Option<&'a str>,
    sample: Option<&'a str>,
    image: Option<&'a str>,
    visible: Option<&'a str>,
    license: Option<&'a str>,
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
) -> HashMap<String, PackFiles<'a>> {
    let mut files: HashMap<String, PackFiles> = HashMap::new();
    for ((path, kind), role) in paths.iter().zip(kinds).zip(roles) {
        let (dir, _) = split_dir(path);
        let Some(pack) = std::iter::once(dir)
            .chain(ancestors(dir).collect::<Vec<_>>().into_iter().rev())
            .find(|d| packs.contains(d))
        else {
            continue;
        };
        let entry = files.entry(pack.to_string()).or_default();
        if role.is_none() {
            first(&mut entry.visible, path);
        }
        if kind.is_image {
            first(&mut entry.image, path);
            if *role == Some(SupportRole::Cover) {
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

/// The pack name a Kenney license header gives: its first line (`Car Kit (3.1)`) without the
/// version in parentheses, when the next line names the author (`Created/distributed by Kenney`
/// or `by Kenney Vleugels`). `None` for any other text, such as an MIT or CC license.
pub fn kenney_header_name(text: &str) -> Option<String> {
    let mut lines = text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    let title = lines.next()?;
    let byline = lines.next()?.to_lowercase();
    if !(byline.starts_with("created/distributed by") || byline.starts_with("by ")) {
        return None;
    }
    // Drops a version such as ` (3.1)` or ` (1.5A)`, wherever it sits.
    let mut name = title.to_string();
    if let Some(start) = name.find(" (") {
        let rest = &name[start + 2..];
        if rest.starts_with(|c: char| c.is_ascii_digit()) {
            if let Some(len) = rest.find(')') {
                name.replace_range(start..start + 2 + len + 1, "");
            }
        }
    }
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty()).then_some(name)
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
        pack: None,
    }
}

/// One indexed file as the pass reads it.
#[derive(sqlx::FromRow)]
struct FileRow {
    id: String,
    file_path: String,
    support_role: Option<String>,
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
    Option<String>,
);

/// A `pack_json` value as `PackInfo`; one that doesn't parse is rewritten by the pass.
fn parse_pack(json: Option<&str>) -> Option<PackInfo> {
    serde_json::from_str(json?).ok()
}

/// The license files whose header the pass read before, by path, from the current `pack_json`s.
async fn read_licenses(pool: &Pool<Sqlite>) -> sqlx::Result<HashMap<String, LicenseFile>> {
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

/// The header name of license file `file` below `root`, from its last read when its size and
/// mtime haven't changed since, else from its first `HEADER_BYTES` on disk. `Err` when it can't
/// be read, so the pass records no read and tries again next time.
fn license_header(
    root: &Path,
    file: &FileRow,
    known: &HashMap<String, LicenseFile>,
) -> std::io::Result<Option<String>> {
    use std::io::Read;
    if let Some(l) = known.get(&file.file_path) {
        if (l.size_bytes, l.modified_at) == (file.size_bytes, file.modified_at) {
            return Ok(l.header_name.clone());
        }
    }
    let mut head = Vec::new();
    std::fs::File::open(root.join(&file.file_path))?
        .take(HEADER_BYTES)
        .read_to_end(&mut head)?;
    Ok(kenney_header_name(&String::from_utf8_lossy(&head)))
}

/// Makes `folders` hold exactly `folders`, writing only the rows that differ, in transactions of
/// `BATCH` rows. Returns how many rows it inserted, updated or deleted.
async fn apply_folders(
    pool: &Pool<Sqlite>,
    folders: &BTreeMap<String, Folder>,
) -> sqlx::Result<usize> {
    let rows: Vec<FolderRow> = sqlx::query_as(
        "SELECT path, parent_path, name, is_pack, item_count, hidden_count, pack_json FROM folders",
    )
    .fetch_all(pool)
    .await?;
    let current: HashMap<String, Folder> = rows
        .into_iter()
        .map(
            |(path, parent_path, name, is_pack, item_count, hidden_count, pack_json)| {
                let folder = Folder {
                    parent_path,
                    name,
                    is_pack,
                    item_count,
                    hidden_count,
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
                "INSERT INTO folders
                     (path, parent_path, name, is_pack, item_count, hidden_count, pack_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(path) DO UPDATE SET parent_path = excluded.parent_path,
                     name = excluded.name, is_pack = excluded.is_pack,
                     item_count = excluded.item_count, hidden_count = excluded.hidden_count,
                     pack_json = excluded.pack_json",
            )
            .bind(path)
            .bind(&folder.parent_path)
            .bind(&folder.name)
            .bind(folder.is_pack)
            .bind(folder.item_count)
            .bind(folder.hidden_count)
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

/// Runs the pass on the library DB at `pool`, whose files lie below `root`: reads every indexed
/// path, derives the structure and applies it. It reads a pack's license file only when its size
/// or mtime changed since the last pass. Returns the rows written. The caller holds the library's
/// `index_lock`.
pub async fn rebuild(pool: &Pool<Sqlite>, root: &Path) -> sqlx::Result<usize> {
    let started = Instant::now();
    let files: Vec<FileRow> =
        sqlx::query_as("SELECT id, file_path, support_role, size_bytes, modified_at FROM assets")
            .fetch_all(pool)
            .await?;
    let known = read_licenses(pool).await?;
    let by_path: HashMap<&str, &FileRow> =
        files.iter().map(|f| (f.file_path.as_str(), f)).collect();
    let mut unread = HashSet::new();
    let mut structure = derive(files.iter().map(|f| f.file_path.as_str()), |path| {
        license_header(root, by_path[path], &known)
            .inspect_err(|e| {
                tracing::debug!(target: "structure", "Could not read {path}: {e}");
                unread.insert(path.to_string());
            })
            .ok()
            .flatten()
    });
    // A read license records the size and mtime it was read at; an unread one records none.
    for license in structure
        .folders
        .values_mut()
        .filter_map(|f| f.pack.as_mut()?.license.as_mut())
        .filter(|l| !unread.contains(&l.path))
    {
        let file = by_path[license.path.as_str()];
        (license.size_bytes, license.modified_at) = (file.size_bytes, file.modified_at);
    }
    let written = apply_folders(pool, &structure.folders).await?
        + apply_roles(pool, &files, &structure.roles).await?;
    tracing::info!(
        target: "structure",
        "Structure pass: {} files, {} folders, {} packs, {} support files, {} rows written in {:?}",
        files.len(),
        structure.folders.len(),
        structure.folders.values().filter(|f| f.is_pack).count(),
        structure.roles.iter().flatten().count(),
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

    /// The structure of `paths` when no license file has a header.
    fn derive<'a>(paths: impl IntoIterator<Item = &'a str>) -> Structure {
        super::derive(paths, |_| None)
    }

    fn folder(parent_path: Option<&str>, name: &str, item_count: i64, hidden_count: i64) -> Folder {
        Folder {
            parent_path: parent_path.map(str::to_string),
            name: name.to_string(),
            is_pack: false,
            item_count,
            hidden_count,
            pack: None,
        }
    }

    /// `folder` as a pack named after its folder, with `cover` and `license`.
    fn pack(folder: Folder, cover: Option<&str>, license: Option<&str>) -> Folder {
        Folder {
            is_pack: true,
            pack: Some(PackInfo {
                name: folder.name.clone(),
                cover: cover.map(str::to_string),
                license: license.map(|path| LicenseFile {
                    path: path.to_string(),
                    header_name: None,
                    size_bytes: 0,
                    modified_at: 0,
                }),
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

    /// A Kenney-like layout: two kits in one category, one sharing the other's name as a prefix,
    /// a file at the root and a file directly in a category.
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
        let expected = BTreeMap::from(
            [
                ("", folder(None, "", 6, 1)),
                ("2D assets", folder(Some(""), "2D assets", 1, 0)),
                (
                    "2D assets/UI Pack",
                    folder(Some("2D assets"), "UI Pack", 1, 0),
                ),
                (
                    "2D assets/UI Pack/PNG",
                    folder(Some("2D assets/UI Pack"), "PNG", 1, 0),
                ),
                ("3D assets", folder(Some(""), "3D assets", 4, 1)),
                (
                    "3D assets/Platformer Kit",
                    pack(
                        folder(Some("3D assets"), "Platformer Kit", 2, 1),
                        Some("3D assets/Platformer Kit/Models/GLB format/block.glb"),
                        Some("3D assets/Platformer Kit/License.txt"),
                    ),
                ),
                (
                    "3D assets/Platformer Kit 2",
                    folder(Some("3D assets"), "Platformer Kit 2", 1, 0),
                ),
                (
                    "3D assets/Platformer Kit/Models",
                    folder(Some("3D assets/Platformer Kit"), "Models", 2, 0),
                ),
                (
                    "3D assets/Platformer Kit/Models/GLB format",
                    folder(Some("3D assets/Platformer Kit/Models"), "GLB format", 1, 0),
                ),
                (
                    "3D assets/Platformer Kit/Models/OBJ format",
                    folder(Some("3D assets/Platformer Kit/Models"), "OBJ format", 1, 0),
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
        assert_eq!(
            derive(reversed),
            Structure {
                roles,
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
        let kit = &structure.folders["3D assets/Car Kit"];
        assert_eq!((kit.item_count, kit.hidden_count), (5, 3));
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

    #[test]
    fn a_packs_name_is_its_license_header_name_else_its_folder_name() {
        let structure = super::derive(KENNEY_3D_KIT, |path| {
            (path == "3D assets/Car Kit/License.txt").then(|| "Car Kit Deluxe".to_string())
        });
        assert_eq!(info(&structure, "3D assets/Car Kit").name, "Car Kit Deluxe");
        assert_eq!(
            info(&structure, "3D assets/Holiday Kit").name,
            "Holiday Kit"
        );
        // The folder keeps its own name in the tree.
        assert_eq!(structure.folders["3D assets/Car Kit"].name, "Car Kit");
    }

    #[test]
    fn the_kenney_header_names_the_pack_without_its_version() {
        let header = |text: &str| kenney_header_name(text);
        let kenney = "\u{feff}\r\n\tCar Kit (3.1)\r\n\r\n\tCreated/distributed by Kenney (www.kenney.nl)\r\n\tCreation date: 02-04-2026\r\n";
        assert_eq!(header(kenney).as_deref(), Some("Car Kit"));
        for (title, name) in [
            ("Input Prompts (1.5A)", "Input Prompts"),
            ("Space Kit (1.2) Exclusive", "Space Kit Exclusive"),
            ("Sketch Desert 1.0", "Sketch Desert 1.0"),
            ("Generic Items #1", "Generic Items #1"),
            ("Platformer Pack (Redux)", "Platformer Pack (Redux)"),
        ] {
            let text = format!("{title}\n\nCreated/distributed by Kenney (www.kenney.nl)\n");
            assert_eq!(header(&text).as_deref(), Some(name), "{title}");
        }
        // The older byline
        assert_eq!(
            header("Roguelike Indoor pack\nby Kenney Vleugels (Kenney.nl)\n---").as_deref(),
            Some("Roguelike Indoor pack")
        );
        // Another license's first line names no pack.
        for text in [
            "MIT License\n\nCopyright (c) 2024 KayKit\n",
            "Creative Commons Zero v1.0 Universal\n\nCC0 1.0 Universal\n",
            "Car Kit (3.1)",
            "",
        ] {
            assert_eq!(header(text), None, "{text:?}");
        }
    }

    /// Every `folders` row, and every asset's support role, in path order.
    #[derive(Debug, PartialEq)]
    pub(crate) struct DerivedRows {
        pub folders: Vec<FolderRow>,
        pub roles: Vec<(String, Option<String>)>,
    }

    /// What the pass has written to `pool`, for comparing a library with a fresh pass.
    pub(crate) async fn derived_rows(pool: &Pool<Sqlite>) -> DerivedRows {
        let folders = sqlx::query_as(
            "SELECT path, parent_path, name, is_pack, item_count, hidden_count, pack_json
             FROM folders ORDER BY path",
        )
        .fetch_all(pool)
        .await
        .unwrap();
        let roles = sqlx::query_as("SELECT file_path, support_role FROM assets ORDER BY file_path")
            .fetch_all(pool)
            .await
            .unwrap();
        DerivedRows { folders, roles }
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

    async fn insert_path(pool: &Pool<Sqlite>, path: &str) {
        sqlx::query(
            "INSERT INTO assets (id, file_path, file_name, extension, kind, size_bytes, modified_at, indexed_at)
             VALUES (?, ?, ?, 'glb', '3d_model', 1, 0, 0)",
        )
        .bind(path)
        .bind(path)
        .bind(path.rsplit('/').next().unwrap())
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
        // 10 folders and the one support file
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 11);
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
    async fn the_pass_names_a_pack_from_its_license_and_rereads_it_only_once_it_changes() {
        let root = tempfile::tempdir().unwrap();
        let license = root.path().join("kit/License.txt");
        std::fs::create_dir_all(license.parent().unwrap()).unwrap();
        std::fs::write(
            &license,
            "\tCar Kit (3.1)\r\n\r\n\tCreated/distributed by Kenney\r\n",
        )
        .unwrap();
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
        assert_eq!(
            (file.header_name.as_deref(), file.size_bytes),
            (Some("Car Kit"), 1)
        );

        // An unchanged row means an unchanged file: the pass doesn't read it again.
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
    }

    #[tokio::test]
    async fn a_count_change_keeps_the_pack_info_and_writes_only_the_counts() {
        let pool = memory_library_pool().await;
        for path in KENNEY {
            insert_path(&pool, path).await;
        }
        rebuild(&pool, unmounted()).await.unwrap();
        insert_path(&pool, "3D assets/Platformer Kit/Models/crate.glb").await;
        // The kit, `Models`, `3D assets` and the root
        assert_eq!(rebuild(&pool, unmounted()).await.unwrap(), 4);
        assert_eq!(
            stored_pack(&pool, "3D assets/Platformer Kit").await,
            info(&derive(KENNEY), "3D assets/Platformer Kit").clone()
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
}
