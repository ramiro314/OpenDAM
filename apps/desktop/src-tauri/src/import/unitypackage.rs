//! The `extract_unitypackage` step (plan pack-import, Steps): unpacks a `.unitypackage` original
//! into a folder tree, ported from `extract_unity.py --keep-all`.
//!
//! A package is a gzipped tar of GUID folders, each holding a `pathname`, and for a file an
//! `asset` and its `asset.meta`. Extraction keeps every file and each `.meta`, drops `Assets/` and
//! a top folder every file shares, and unpacks a nested package into a folder named after it.
//! The package is the original itself, or a variant inside a zip original (`extract_variant`),
//! read from the zip as a stream. The original is only read. Every file it writes is a regular file it creates and fills from an
//! entry's bytes, and it never asks the tar reader to unpack an entry, so no link is ever made.
//!
//! Unsafe entries (absolute or `..` pathnames, links, devices and other special types) and
//! duplicate ones are skipped and reported, and dropped before the shared top folder is found, so
//! they never change the layout. `extract_unity.py` counts an escaping pathname as a second top
//! folder instead (plan R13).
//!
//! `list_unitypackage` and `list_variant` read a package the same way and lay it out with the
//! same code, but keep only each entry's size: an import's scan previews a build from them
//! without writing anything (`super::scan`).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use flate2::read::MultiGzDecoder;
use serde::{Deserialize, Serialize};
use tar::EntryType;

use super::zipfile;

/// A file the extraction wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractedFile {
    /// Relative to the target, `/`-separated.
    pub path: String,
    /// The chain of pathnames that leads to it in the original, outermost first: the raw
    /// `pathname` of each nested package, then its own. A `.meta` file's is its asset's with
    /// `.meta` appended to the last element.
    pub source_path: Vec<String>,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkipReason {
    /// An absolute pathname.
    Absolute,
    /// A pathname with a `..` component.
    Escapes,
    /// A pathname that names nothing once `Assets/` is dropped.
    NoName,
    /// A pathname the package already holds, one ending in `.meta`, or a repeated tar member.
    Duplicate,
    Hardlink,
    Symlink,
    /// A device, a FIFO or any other entry that isn't a regular file or a folder.
    Special,
    /// An Unreal asset the editor's glTF exporter failed on, named by its `/Game` path.
    NotExported,
}

/// An entry the extraction skipped, for the run's warnings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedEntry {
    /// The entry's source path, or for a tar member with no known `pathname`, the chain ending in
    /// the member's own name.
    pub source_path: Vec<String>,
    pub reason: SkipReason,
}

#[derive(Debug, Default)]
pub struct Extraction {
    /// Sorted by path.
    pub files: Vec<ExtractedFile>,
    /// In the order they were found.
    pub skipped: Vec<SkippedEntry>,
}

/// Extracts `package` into `target`, which must be empty or missing. With a `subpath` (a folder
/// of the extracted layout, such as a render pipeline's), only the files below it are written,
/// relative to it. A clash, a file that would land on another, fails the extraction. A failed
/// extraction leaves the target as it found it: empty, or missing.
pub fn extract_unitypackage(
    package: &Path,
    target: &Path,
    subpath: Option<&str>,
) -> Result<Extraction> {
    let subpath = clean_subpath(subpath)?;
    into_empty(target, |target| {
        let original =
            File::open(package).with_context(|| format!("opening {}", package.display()))?;
        extract_into(original, &[], target, subpath.as_deref())
    })
}

/// Extracts the variant `.unitypackage` at `variant` inside the zip `original`, as
/// `extract_unitypackage` does. The variant's path in the zip starts every source path. The
/// package is read straight from the zip, never written out whole.
pub fn extract_variant(
    original: &Path,
    variant: &str,
    target: &Path,
    subpath: Option<&str>,
) -> Result<Extraction> {
    let subpath = clean_subpath(subpath)?;
    into_empty(target, |target| {
        let mut zip = zipfile::open(original)?;
        let (index, name) = zipfile::variant(&mut zip, variant)?;
        let package = zip
            .by_index(index)
            .with_context(|| format!("reading {name} in the zip"))?;
        extract_into(package, &[name], target, subpath.as_deref())
    })
}

/// A sub-folder option's clean form.
pub(super) fn clean_subpath(subpath: Option<&str>) -> Result<Option<String>> {
    match subpath {
        Some(raw) => match clean(raw) {
            Ok(parts) if !parts.is_empty() => Ok(Some(parts.join("/"))),
            _ => bail!("invalid sub-folder {raw:?}"),
        },
        None => Ok(None),
    }
}

/// Runs `extract` into `target`, which must be empty or missing. When it fails, the target is
/// left as it was found: empty, or missing.
pub(super) fn into_empty(
    target: &Path,
    extract: impl FnOnce(&Path) -> Result<Extraction>,
) -> Result<Extraction> {
    let missing = !target.exists();
    fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    if fs::read_dir(target)?.next().is_some() {
        bail!("{} is not empty", target.display());
    }
    let result = extract(target);
    if result.is_err() {
        // The target held nothing before, so everything in it is this call's.
        if let Err(err) = clear(target, missing) {
            tracing::warn!(
                "clearing {} after a failed extraction: {err}",
                target.display()
            );
        }
    }
    result
}

/// Lists the files `extract_unitypackage` would write for `package`, with their source paths,
/// sizes and the entries it would skip, from the package's index alone: its `pathname` entries
/// and its entry headers. No asset's bytes are kept and nothing is written. A nested package is
/// listed from its stream as the outer one is read. It fails where the extraction would.
pub fn list_unitypackage(package: &Path, subpath: Option<&str>) -> Result<Extraction> {
    let subpath = clean_subpath(subpath)?;
    let mut original =
        File::open(package).with_context(|| format!("opening {}", package.display()))?;
    run(
        &mut Listing::default(),
        &mut original,
        &[],
        subpath.as_deref(),
    )
}

/// Lists the variant `.unitypackage` at `variant` inside the zip `original`, as
/// `list_unitypackage` does, reading it straight from the zip.
pub fn list_variant(original: &Path, variant: &str, subpath: Option<&str>) -> Result<Extraction> {
    let subpath = clean_subpath(subpath)?;
    let mut zip = zipfile::open(original)?;
    let (index, name) = zipfile::variant(&mut zip, variant)?;
    let mut package = zip
        .by_index(index)
        .with_context(|| format!("reading {name} in the zip"))?;
    run(
        &mut Listing::default(),
        &mut package,
        &[name],
        subpath.as_deref(),
    )
}

/// Unpacks the package read from `package`, whose own source path is `chain` (empty for the
/// original itself), into `target`.
fn extract_into(
    mut package: impl Read,
    chain: &[String],
    target: &Path,
    subpath: Option<&str>,
) -> Result<Extraction> {
    // Entries are staged here, by number, until the pathnames are known, then renamed into place.
    let scratch = tempfile::Builder::new()
        .prefix(".extract-")
        .tempdir_in(target)?;
    let mut disk = Disk {
        target,
        scratch: scratch.path(),
        scratch_name: scratch
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        blobs: 0,
    };
    let out = run(&mut disk, &mut package, chain, subpath)?;
    scratch.close()?;
    Ok(out)
}

/// Reads the package and lays it out through `sink`, sorted by path.
fn run<S: Sink>(
    sink: &mut S,
    package: &mut dyn Read,
    chain: &[String],
    subpath: Option<&str>,
) -> Result<Extraction> {
    let mut out = Extraction::default();
    let read = read_package(sink, package)?;
    lay_out(sink, read, chain, subpath, "", &mut out)?;
    if let (Some(subpath), true) = (subpath, out.files.is_empty()) {
        bail!("the package holds no file under {subpath}");
    }
    out.files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Empties `target`, or removes it when `remove` is set.
fn clear(target: &Path, remove: bool) -> io::Result<()> {
    if remove {
        return fs::remove_dir_all(target);
    }
    for entry in fs::read_dir(target)? {
        let path = entry?.path();
        if path.symlink_metadata()?.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// Where a package's entries go: written below a target (`Disk`), or only listed (`Listing`).
/// The reading and the layout are the same for both, so a listing names exactly the files an
/// extraction writes.
trait Sink: Sized {
    /// An asset's or `.meta`'s bytes, as the sink keeps them.
    type Blob;
    fn take(&mut self, data: &mut dyn Read, size: u64, mtime: u64) -> Result<Self::Blob>;
    fn size(blob: &Self::Blob) -> u64;
    /// The nested package whose bytes are `blob`, read.
    fn nested(&mut self, blob: Self::Blob) -> Result<Package<Self::Blob>>;
    /// Puts `blob` at `rel` below the target, refusing to replace anything there.
    fn put(&mut self, blob: Self::Blob, rel: &str) -> Result<()>;
}

/// Writes each entry into a scratch folder, then renames it into place.
struct Disk<'a> {
    target: &'a Path,
    scratch: &'a Path,
    scratch_name: String,
    blobs: u64,
}

/// An entry's bytes, staged in the scratch folder with the entry's mtime.
struct Blob {
    file: PathBuf,
    size: u64,
}

impl Sink for Disk<'_> {
    type Blob = Blob;

    fn take(&mut self, data: &mut dyn Read, _size: u64, mtime: u64) -> Result<Blob> {
        self.blobs += 1;
        let file = self.scratch.join(self.blobs.to_string());
        let mut staged = File::create_new(&file)?;
        let size = io::copy(data, &mut staged)?;
        if let Some(mtime) = UNIX_EPOCH.checked_add(Duration::from_secs(mtime)) {
            staged.set_modified(mtime)?;
        }
        Ok(Blob { file, size })
    }

    fn size(blob: &Blob) -> u64 {
        blob.size
    }

    fn nested(&mut self, blob: Blob) -> Result<Package<Blob>> {
        let mut file = File::open(&blob.file)?;
        let package = read_package(self, &mut file)?;
        fs::remove_file(&blob.file)?;
        Ok(package)
    }

    fn put(&mut self, blob: Blob, rel: &str) -> Result<()> {
        if rel.split('/').next() == Some(self.scratch_name.as_str()) {
            bail!("{rel} would land in the extraction's scratch folder");
        }
        let to = self.target.join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating the folder of {rel}"))?;
        }
        if to.symlink_metadata().is_ok() {
            bail!("{rel} would overwrite a file the package already wrote");
        }
        fs::rename(&blob.file, &to).with_context(|| format!("writing {rel}"))
    }
}

/// Keeps each entry's size, and lists a gzipped entry as a nested package while it is read,
/// since its `pathname` may come later and the stream can't be read again.
#[derive(Default)]
struct Listing {
    taken: Taken,
}

struct Listed {
    size: u64,
    /// The entry read as a package, when it is gzipped.
    nested: Option<Result<Package<Listed>>>,
}

impl Sink for Listing {
    type Blob = Listed;

    fn take(&mut self, data: &mut dyn Read, size: u64, _mtime: u64) -> Result<Listed> {
        let mut magic = [0u8; 2];
        let mut read = 0;
        while read < magic.len() {
            match data.read(&mut magic[read..])? {
                0 => break,
                n => read += n,
            }
        }
        let nested = (magic[..read] == [0x1f, 0x8b]).then(|| {
            let mut whole = io::Cursor::new(magic).chain(data);
            read_package(self, &mut whole)
        });
        Ok(Listed { size, nested })
    }

    fn size(blob: &Listed) -> u64 {
        blob.size
    }

    fn nested(&mut self, blob: Listed) -> Result<Package<Listed>> {
        match blob.nested {
            Some(package) => package,
            None => bail!("it isn't a gzipped package"),
        }
    }

    fn put(&mut self, _blob: Listed, rel: &str) -> Result<()> {
        self.taken.claim(rel, "package")
    }
}

/// The paths a listing has placed, to refuse what an extraction couldn't write: a file where
/// one already is, or below a file.
#[derive(Default)]
pub(super) struct Taken {
    files: HashSet<String>,
    folders: HashSet<String>,
}

impl Taken {
    pub(super) fn claim(&mut self, rel: &str, what: &str) -> Result<()> {
        let folders: Vec<&str> = rel.match_indices('/').map(|(i, _)| &rel[..i]).collect();
        if folders.iter().any(|f| self.files.contains(*f)) {
            bail!("creating the folder of {rel}: a file is in the way");
        }
        if self.files.contains(rel) || self.folders.contains(rel) {
            bail!("{rel} would overwrite a file the {what} already wrote");
        }
        self.folders.extend(folders.into_iter().map(str::to_string));
        self.files.insert(rel.to_string());
        Ok(())
    }
}

struct Guid<B> {
    pathname: Option<String>,
    asset: Option<B>,
    meta: Option<B>,
}

/// One package's files, after the unsafe and duplicate ones are dropped, and its skipped entries
/// as `(name, reason)`, the name not yet chained to the package's own source path.
struct Package<B> {
    items: Vec<Item<B>>,
    skipped: Vec<(String, SkipReason)>,
}

struct Item<B> {
    pathname: String,
    /// Its layout below the package's destination: `Assets/` dropped, and later the shared top.
    parts: Vec<String>,
    asset: B,
    meta: Option<B>,
}

/// Reads one package's entries through `sink`.
fn read_package<S: Sink>(sink: &mut S, reader: &mut dyn Read) -> Result<Package<S::Blob>> {
    // Read every entry first: a GUID's `pathname` can come after its `asset`.
    let mut order: Vec<String> = Vec::new();
    let mut guids: HashMap<String, Guid<S::Blob>> = HashMap::new();
    let mut members = HashSet::new();
    let mut odd: Vec<(String, SkipReason)> = Vec::new();
    let mut archive = tar::Archive::new(MultiGzDecoder::new(reader));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        if kind.is_dir() || kind == EntryType::XGlobalHeader {
            continue;
        }
        // Python's tarfile counts these three as regular files, and `extract_unity.py` reads
        // only regular files.
        if !matches!(
            kind,
            EntryType::Regular | EntryType::Continuous | EntryType::GNUSparse
        ) {
            let reason = if kind.is_hard_link() {
                SkipReason::Hardlink
            } else if kind.is_symlink() {
                SkipReason::Symlink
            } else {
                SkipReason::Special
            };
            odd.push((name, reason));
            continue;
        }
        let Some((guid, part)) = member_parts(&name) else {
            continue;
        };
        if !matches!(part, "pathname" | "asset" | "asset.meta") {
            continue; // `preview.png` and the like
        }
        // Keyed by GUID and part, so `./g/asset` repeats `g/asset`.
        if !members.insert((guid.to_string(), part.to_string())) {
            odd.push((name, SkipReason::Duplicate));
            continue;
        }
        if !guids.contains_key(guid) {
            order.push(guid.to_string());
        }
        let slot = guids.entry(guid.to_string()).or_insert(Guid {
            pathname: None,
            asset: None,
            meta: None,
        });
        if part == "pathname" {
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            let text = String::from_utf8_lossy(&data);
            slot.pathname = Some(text.lines().next().unwrap_or("").trim().to_string());
            continue;
        }
        let (size, mtime) = (entry.size(), entry.header().mtime()?);
        let blob = Some(sink.take(&mut entry, size, mtime)?);
        if part == "asset" {
            slot.asset = blob;
        } else {
            slot.meta = blob;
        }
    }

    // A member's source path is its GUID's pathname when the package names one.
    let mut skipped = Vec::new();
    for (name, reason) in odd {
        let named = member_parts(&name).and_then(|(guid, part)| {
            let pathname = guids.get(guid)?.pathname.as_ref()?;
            match part {
                "asset" => Some(pathname.clone()),
                "asset.meta" => Some(format!("{pathname}.meta")),
                _ => None,
            }
        });
        skipped.push((named.unwrap_or(name), reason));
    }

    // A GUID with no asset is a folder, and an asset with no pathname has nowhere to go.
    let mut items = Vec::new();
    let mut pathnames = HashSet::new();
    for guid in order {
        let slot = guids.remove(&guid).unwrap();
        let (Some(pathname), Some(asset)) = (slot.pathname, slot.asset) else {
            continue;
        };
        let parts = layout(&pathname).and_then(|parts| {
            // A `.meta` pathname would collide with an asset's own `.meta`.
            if pathname.ends_with(".meta") || !pathnames.insert(pathname.clone()) {
                Err(SkipReason::Duplicate)
            } else {
                Ok(parts)
            }
        });
        match parts {
            Ok(parts) => items.push(Item {
                pathname,
                parts,
                asset,
                meta: slot.meta,
            }),
            Err(reason) => skipped.push((pathname, reason)),
        }
    }
    Ok(Package { items, skipped })
}

/// Lays one package (the original, or a nested one) out into `dest` below the target, through
/// `sink`. `chain` is the source path of the package itself, empty for the original.
fn lay_out<S: Sink>(
    sink: &mut S,
    package: Package<S::Blob>,
    chain: &[String],
    subpath: Option<&str>,
    dest: &str,
    out: &mut Extraction,
) -> Result<()> {
    let source = |name: &str| -> Vec<String> {
        let mut path = chain.to_vec();
        path.push(name.to_string());
        path
    };
    for (name, reason) in package.skipped {
        out.skipped.push(SkippedEntry {
            source_path: source(&name),
            reason,
        });
    }

    // Drop a top folder every file shares.
    let mut items = package.items;
    let top = items.first().map(|item| item.parts[0].clone());
    if items
        .iter()
        .all(|item| item.parts.len() > 1 && Some(&item.parts[0]) == top.as_ref())
    {
        for item in &mut items {
            item.parts.remove(0);
        }
    }

    for item in items {
        let path = item.parts.join("/");
        if let Some(folder) = nested_folder(&item.parts) {
            // A nested package becomes a folder named after it, where it sat. Its own `.meta`
            // describes a file that isn't written, so it's dropped, as `extract_unity.py` does.
            let (inner_subpath, inner_dest) = match subpath {
                None => (None, folder),
                Some(sub) => match below(&folder, sub) {
                    Some(rel) => (None, rel.to_string()),
                    None => match below(sub, &folder) {
                        Some(rel) if !rel.is_empty() => (Some(rel), String::new()),
                        _ => continue,
                    },
                },
            };
            let inner_chain = source(&item.pathname);
            sink.nested(item.asset)
                .and_then(|nested| {
                    let dest = join(dest, &inner_dest);
                    lay_out(sink, nested, &inner_chain, inner_subpath, &dest, out)
                })
                .with_context(|| format!("unpacking the nested package {}", item.pathname))?;
            continue;
        }
        let rel = match subpath {
            None => path.as_str(),
            Some(sub) => match below(&path, sub) {
                Some(rel) if !rel.is_empty() => rel,
                _ => continue,
            },
        };
        let rel = join(dest, rel);
        let size = S::size(&item.asset);
        sink.put(item.asset, &rel)?;
        out.files.push(ExtractedFile {
            path: rel.clone(),
            source_path: source(&item.pathname),
            size,
        });
        if let Some(meta) = item.meta {
            let rel = format!("{rel}.meta");
            let size = S::size(&meta);
            sink.put(meta, &rel)?;
            out.files.push(ExtractedFile {
                path: rel,
                source_path: source(&format!("{}.meta", item.pathname)),
                size,
            });
        }
    }
    Ok(())
}

/// A tar member's GUID and file name, when it has exactly those two parts.
fn member_parts(name: &str) -> Option<(&str, &str)> {
    if name.starts_with('/') {
        return None;
    }
    let mut parts = name.split('/').filter(|p| !p.is_empty() && *p != ".");
    match (parts.next(), parts.next(), parts.next()) {
        (Some(guid), Some(part), None) => Some((guid, part)),
        _ => None,
    }
}

/// A relative path's components, without empty and `.` ones.
pub(super) fn clean(path: &str) -> std::result::Result<Vec<String>, SkipReason> {
    if path.starts_with('/') {
        return Err(SkipReason::Absolute);
    }
    let parts: Vec<String> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .map(str::to_string)
        .collect();
    if parts.iter().any(|p| p == "..") {
        return Err(SkipReason::Escapes);
    }
    Ok(parts)
}

/// A pathname's place in the layout, `Assets/` dropped.
fn layout(pathname: &str) -> std::result::Result<Vec<String>, SkipReason> {
    let mut parts = clean(pathname)?;
    if parts.first().map(String::as_str) == Some("Assets") {
        parts.remove(0);
    }
    if parts.is_empty() {
        return Err(SkipReason::NoName);
    }
    Ok(parts)
}

/// The folder a nested package unpacks into: its path without the `.unitypackage` extension.
fn nested_folder(parts: &[String]) -> Option<String> {
    let (name, rest) = parts.split_last()?;
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || !ext.eq_ignore_ascii_case("unitypackage") {
        return None;
    }
    Some(join(&rest.join("/"), stem))
}

/// `path` relative to `folder` when it is the folder or below it (`""` for the folder itself).
pub(super) fn below<'p>(path: &'p str, folder: &str) -> Option<&'p str> {
    match path.strip_prefix(folder)? {
        "" => Some(""),
        rest => rest.strip_prefix('/'),
    }
}

pub(super) fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}/{b}"),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    pub(in crate::import) fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pack-import")
            .join(name)
    }

    pub(in crate::import) fn golden(name: &str) -> Vec<String> {
        fs::read_to_string(fixture("expected").join(name))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Every path below `root`, sorted, checking that each is a regular file or a folder with
    /// one link.
    pub(in crate::import) fn on_disk(root: &Path) -> Vec<String> {
        let mut files = Vec::new();
        for entry in walkdir::WalkDir::new(root).min_depth(1) {
            let entry = entry.unwrap();
            let meta = entry.path().symlink_metadata().unwrap();
            assert!(
                meta.is_dir() || (meta.is_file() && meta.nlink() == 1),
                "{} is neither a folder nor a regular file with one link",
                entry.path().display()
            );
            if meta.is_file() {
                let rel = entry.path().strip_prefix(root).unwrap();
                files.push(rel.to_string_lossy().into_owned());
            }
        }
        files.sort();
        files
    }

    fn paths(extraction: &Extraction) -> Vec<String> {
        extraction.files.iter().map(|f| f.path.clone()).collect()
    }

    fn source_of<'e>(extraction: &'e Extraction, path: &str) -> &'e [String] {
        let file = extraction.files.iter().find(|f| f.path == path);
        &file
            .unwrap_or_else(|| panic!("{path} not extracted"))
            .source_path
    }

    #[test]
    fn synthetic_extracts_as_its_golden() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("Pack");
        let extraction =
            extract_unitypackage(&fixture("synthetic.unitypackage"), &target, None).unwrap();

        let expected = golden("synthetic.extract.txt");
        assert_eq!(paths(&extraction), expected);
        assert_eq!(on_disk(&target), expected);
        assert!(extraction.skipped.is_empty(), "{:?}", extraction.skipped);
        for file in &extraction.files {
            assert_eq!(
                fs::metadata(target.join(&file.path)).unwrap().len(),
                file.size
            );
        }
        // Each file gets its entry's mtime, and the original's bytes.
        let fbx = target.join("Built-In/Built-In_Pack/Pack/Art/Models/SM_Log.fbx");
        let mtime = fs::metadata(&fbx).unwrap().modified().unwrap();
        assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        let source = fixture("../game-export/library/Unity Moss/Models/SM_Log.fbx");
        assert_eq!(fs::read(fbx).unwrap(), fs::read(source).unwrap());
    }

    #[test]
    fn source_paths_are_unique_across_nesting() {
        let tmp = tempfile::tempdir().unwrap();
        let extraction =
            extract_unitypackage(&fixture("synthetic.unitypackage"), tmp.path(), None).unwrap();

        let rock = "Assets/Pack/Built-In/Built-In_Pack/Pack/Art/T_Rock.png";
        let houses = "Assets/Pack/Built-In/Built-In_Pack/Pack/Houses.unitypackage";
        assert_eq!(
            source_of(&extraction, "Built-In/Built-In_Pack/Pack/Art/T_Rock.png"),
            [rock]
        );
        assert_eq!(
            source_of(
                &extraction,
                "Built-In/Built-In_Pack/Pack/Houses/Built-In/Built-In_Pack/Pack/Art/T_Rock.png"
            ),
            [houses, rock]
        );

        // Every `.meta` has its asset's source path with `.meta` appended.
        let mut metas = 0;
        for file in &extraction.files {
            let Some(asset) = file.path.strip_suffix(".meta") else {
                continue;
            };
            let mut expected = source_of(&extraction, asset).to_vec();
            expected.last_mut().unwrap().push_str(".meta");
            assert_eq!(file.source_path, expected, "{}", file.path);
            metas += 1;
        }
        assert_eq!(metas, 8);

        let keys: HashSet<_> = extraction.files.iter().map(|f| &f.source_path).collect();
        assert_eq!(keys.len(), extraction.files.len());
    }

    #[test]
    fn unsafe_entries_are_skipped_and_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("Pack");
        let absolute = Path::new("/tmp/opendam-pack-import-absolute.txt");
        let _ = fs::remove_file(absolute);
        let extraction =
            extract_unitypackage(&fixture("escape.unitypackage"), &target, None).unwrap();

        // The shared top folder `Esc/` is still dropped, and the first `b.txt` wins.
        let expected = ["Keep/a.txt", "Keep/a.txt.meta", "b.txt", "b.txt.meta"];
        assert_eq!(paths(&extraction), expected);
        assert_eq!(on_disk(&target), expected);
        assert_eq!(fs::read(target.join("b.txt")).unwrap(), b"first b\n");

        let skipped: Vec<(&str, SkipReason)> = extraction
            .skipped
            .iter()
            .map(|s| {
                assert_eq!(s.source_path.len(), 1);
                (s.source_path[0].as_str(), s.reason)
            })
            .collect();
        assert_eq!(
            skipped,
            [
                ("Assets/Esc/hardlink.txt", SkipReason::Hardlink),
                ("Assets/Esc/symlink.txt", SkipReason::Symlink),
                ("../escape.txt", SkipReason::Escapes),
                (absolute.to_str().unwrap(), SkipReason::Absolute),
                ("Assets/Esc/b.txt", SkipReason::Duplicate),
                ("Assets/Esc/b.txt.meta", SkipReason::Duplicate),
            ]
        );

        // Nothing lands outside the target.
        assert_eq!(on_disk(tmp.path()).len(), expected.len());
        assert!(!absolute.exists());
        assert!(!tmp.path().join("escape.txt").exists());
    }

    #[test]
    fn the_original_is_untouched() {
        for name in ["synthetic.unitypackage", "escape.unitypackage"] {
            let original = fixture(name);
            let bytes = fs::read(&original).unwrap();
            let mtime = fs::metadata(&original).unwrap().modified().unwrap();
            let tmp = tempfile::tempdir().unwrap();
            extract_unitypackage(&original, tmp.path(), None).unwrap();
            assert_eq!(fs::read(&original).unwrap(), bytes, "{name}");
            assert_eq!(
                fs::metadata(&original).unwrap().modified().unwrap(),
                mtime,
                "{name}"
            );
        }
    }

    #[test]
    fn a_subpath_extracts_one_folder() {
        let synthetic = fixture("synthetic.unitypackage");
        let tmp = tempfile::tempdir().unwrap();
        let art = tmp.path().join("art");
        let extraction =
            extract_unitypackage(&synthetic, &art, Some("Built-In/Built-In_Pack/Pack/Art"))
                .unwrap();
        let expected: Vec<String> = golden("synthetic.extract.txt")
            .iter()
            .filter_map(|p| p.strip_prefix("Built-In/Built-In_Pack/Pack/Art/"))
            .map(str::to_string)
            .collect();
        assert_eq!(paths(&extraction), expected);
        assert_eq!(on_disk(&art), expected);

        // A sub-folder inside a nested package, and one that holds the nested package's folder.
        let inner = tmp.path().join("inner");
        let houses = "Built-In/Built-In_Pack/Pack/Houses";
        let extraction =
            extract_unitypackage(&synthetic, &inner, Some(&format!("{houses}/Built-In/"))).unwrap();
        assert_eq!(
            paths(&extraction),
            [
                "Built-In_Pack/Pack/Art/Materials/M_Rock.mat",
                "Built-In_Pack/Pack/Art/Materials/M_Rock.mat.meta",
                "Built-In_Pack/Pack/Art/T_Rock.png",
                "Built-In_Pack/Pack/Art/T_Rock.png.meta",
            ]
        );
        let outer = tmp.path().join("outer");
        let extraction =
            extract_unitypackage(&synthetic, &outer, Some("Built-In/Built-In_Pack/Pack")).unwrap();
        assert!(paths(&extraction)
            .contains(&"Houses/Built-In/Built-In_Pack/Pack/Art/T_Rock.png".into()));

        let err =
            extract_unitypackage(&synthetic, &tmp.path().join("none"), Some("URP")).unwrap_err();
        assert!(err.to_string().contains("no file under URP"), "{err}");
        let err =
            extract_unitypackage(&synthetic, &tmp.path().join("up"), Some("../x")).unwrap_err();
        assert!(err.to_string().contains("invalid sub-folder"), "{err}");
    }

    /// A gzipped tar of `(member, type, bytes)` entries.
    fn package(entries: &[(&str, EntryType, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, kind, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            // The raw name, which `append_data` would normalize.
            header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            tar.append(&header, *data).unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        io::Write::write_all(&mut gz, &tar.into_inner().unwrap()).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn special_entries_are_skipped_and_clashes_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("special.unitypackage");
        fs::write(
            &original,
            package(&[
                ("g1/asset", EntryType::Fifo, b""),
                ("g1/pathname", EntryType::Regular, b"Assets/P/fifo"),
                ("g2/asset", EntryType::Char, b""),
                ("g2/pathname", EntryType::Regular, b"Assets/P/null"),
                ("g3/asset", EntryType::Regular, b"x"),
                ("g3/pathname", EntryType::Regular, b"Assets/P/x.txt"),
            ]),
        )
        .unwrap();
        let extraction = extract_unitypackage(&original, &tmp.path().join("t"), None).unwrap();
        assert_eq!(paths(&extraction), ["x.txt"]);
        let reasons: Vec<_> = extraction.skipped.iter().map(|s| s.reason).collect();
        assert_eq!(reasons, [SkipReason::Special, SkipReason::Special]);

        // `Assets/P/x.txt` and `P/x.txt` land on the same file.
        fs::write(
            &original,
            package(&[
                ("g1/asset", EntryType::Regular, b"1"),
                ("g1/pathname", EntryType::Regular, b"Assets/P/x.txt"),
                ("g2/asset", EntryType::Regular, b"2"),
                ("g2/pathname", EntryType::Regular, b"P/x.txt"),
            ]),
        )
        .unwrap();
        let missing = tmp.path().join("u");
        let err = extract_unitypackage(&original, &missing, None).unwrap_err();
        assert!(err.to_string().contains("x.txt would overwrite"), "{err}");

        // A failed extraction leaves the target as it found it, so it can be retried.
        assert!(!missing.exists());
        let empty = tmp.path().join("v");
        fs::create_dir(&empty).unwrap();
        extract_unitypackage(&original, &empty, None).unwrap_err();
        assert_eq!(fs::read_dir(&empty).unwrap().count(), 0);
    }

    #[test]
    fn a_repeated_member_is_skipped_however_it_is_spelled() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("repeat.unitypackage");
        fs::write(
            &original,
            package(&[
                ("g1/asset", EntryType::Regular, b"first"),
                ("g1/pathname", EntryType::Regular, b"Assets/P/x.txt"),
                ("./g1/asset", EntryType::Regular, b"second"),
                ("g1//pathname", EntryType::Regular, b"Assets/P/moved.txt"),
            ]),
        )
        .unwrap();
        let target = tmp.path().join("t");
        let extraction = extract_unitypackage(&original, &target, None).unwrap();
        assert_eq!(paths(&extraction), ["x.txt"]);
        assert_eq!(fs::read(target.join("x.txt")).unwrap(), b"first");
        let skipped: Vec<_> = extraction
            .skipped
            .iter()
            .map(|s| (s.source_path.clone(), s.reason))
            .collect();
        assert_eq!(
            skipped,
            [
                (vec!["Assets/P/x.txt".to_string()], SkipReason::Duplicate),
                (vec!["g1//pathname".to_string()], SkipReason::Duplicate),
            ]
        );
    }

    #[test]
    fn a_top_folder_is_dropped_only_when_every_file_shares_it() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("tops.unitypackage");
        let cases: [(&[u8], &[&str]); 2] = [
            // Two top folders.
            (b"Assets/B/y.txt", &["A/x.txt", "B/y.txt"]),
            // A file at the top level.
            (b"Assets/z.txt", &["A/x.txt", "z.txt"]),
        ];
        for (i, (second, expected)) in cases.into_iter().enumerate() {
            fs::write(
                &original,
                package(&[
                    ("g1/asset", EntryType::Regular, b"x"),
                    ("g1/pathname", EntryType::Regular, b"Assets/A/x.txt"),
                    ("g2/asset", EntryType::Regular, b"y"),
                    ("g2/pathname", EntryType::Regular, second),
                ]),
            )
            .unwrap();
            let target = tmp.path().join(i.to_string());
            let extraction = extract_unitypackage(&original, &target, None).unwrap();
            assert_eq!(paths(&extraction), expected);
            assert_eq!(on_disk(&target), expected);
        }
    }

    #[test]
    fn a_target_that_is_not_empty_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("mine.txt"), b"mine").unwrap();
        let err =
            extract_unitypackage(&fixture("synthetic.unitypackage"), tmp.path(), None).unwrap_err();
        assert!(err.to_string().contains("is not empty"), "{err}");
        assert_eq!(on_disk(tmp.path()), ["mine.txt"]);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }
}
