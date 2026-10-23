//! The `extract_unitypackage` step (plan pack-import, Steps): unpacks a `.unitypackage` original
//! into a folder tree, ported from `extract_unity.py --keep-all`.
//!
//! A package is a gzipped tar of GUID folders, each holding a `pathname`, and for a file an
//! `asset` and its `asset.meta`. Extraction keeps every file and each `.meta`, drops `Assets/` and
//! a top folder every file shares, and unpacks a nested package into a folder named after it.
//! The original is only read. Every file it writes is a regular file it creates and fills from an
//! entry's bytes, and it never asks the tar reader to unpack an entry, so no link is ever made.
//!
//! Unsafe entries (absolute or `..` pathnames, links, devices and other special types) and
//! duplicate ones are skipped and reported, and dropped before the shared top folder is found, so
//! they never change the layout. `extract_unity.py` counts an escaping pathname as a second top
//! folder instead (plan R13).

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use flate2::read::MultiGzDecoder;
use serde::Serialize;
use tar::EntryType;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
}

/// An entry the extraction skipped, for the run's warnings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
/// relative to it. A clash, a file that would land on another, fails the extraction.
pub fn extract_unitypackage(
    package: &Path,
    target: &Path,
    subpath: Option<&str>,
) -> Result<Extraction> {
    let subpath = match subpath {
        Some(raw) => match clean(raw) {
            Ok(parts) if !parts.is_empty() => Some(parts.join("/")),
            _ => bail!("invalid sub-folder {raw:?}"),
        },
        None => None,
    };
    fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    if fs::read_dir(target)?.next().is_some() {
        bail!("{} is not empty", target.display());
    }
    // Entries are staged here, by number, until the pathnames are known, then renamed into place.
    let scratch = tempfile::Builder::new()
        .prefix(".extract-")
        .tempdir_in(target)?;
    let mut job = Job {
        target,
        scratch: scratch.path(),
        scratch_name: scratch
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        blobs: 0,
        out: Extraction::default(),
    };
    let original = File::open(package).with_context(|| format!("opening {}", package.display()))?;
    unpack(&mut job, original, &[], subpath.as_deref(), "")?;
    let mut out = job.out;
    scratch.close()?;
    if let (Some(subpath), true) = (&subpath, out.files.is_empty()) {
        bail!("the package holds no file under {subpath}");
    }
    out.files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

struct Job<'a> {
    target: &'a Path,
    scratch: &'a Path,
    scratch_name: String,
    blobs: u64,
    out: Extraction,
}

/// An entry's bytes, staged in the scratch folder with the entry's mtime.
struct Blob {
    file: PathBuf,
    size: u64,
}

#[derive(Default)]
struct Guid {
    pathname: Option<String>,
    asset: Option<Blob>,
    meta: Option<Blob>,
}

/// A file of one package, after the unsafe and duplicate ones are dropped.
struct Item {
    pathname: String,
    /// Its layout below the package's destination: `Assets/` dropped, and later the shared top.
    parts: Vec<String>,
    asset: Blob,
    meta: Option<Blob>,
}

/// Unpacks one package (the original, or a nested one) into `dest` below the target. `chain` is
/// the source path of the package itself, empty for the original.
fn unpack(
    job: &mut Job,
    reader: impl Read,
    chain: &[String],
    subpath: Option<&str>,
    dest: &str,
) -> Result<()> {
    let source = |name: &str| -> Vec<String> {
        let mut path = chain.to_vec();
        path.push(name.to_string());
        path
    };

    // Read every entry first: a GUID's `pathname` can come after its `asset`.
    let mut order: Vec<String> = Vec::new();
    let mut guids: HashMap<String, Guid> = HashMap::new();
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
        if !members.insert(name.clone()) {
            odd.push((name, SkipReason::Duplicate));
            continue;
        }
        if !guids.contains_key(guid) {
            order.push(guid.to_string());
        }
        let slot = guids.entry(guid.to_string()).or_default();
        if part == "pathname" {
            let mut data = Vec::new();
            entry.read_to_end(&mut data)?;
            let text = String::from_utf8_lossy(&data);
            slot.pathname = Some(text.lines().next().unwrap_or("").trim().to_string());
            continue;
        }
        job.blobs += 1;
        let file = job.scratch.join(job.blobs.to_string());
        let mut staged = File::create_new(&file)?;
        let size = io::copy(&mut entry, &mut staged)?;
        if let Some(mtime) = UNIX_EPOCH.checked_add(Duration::from_secs(entry.header().mtime()?)) {
            staged.set_modified(mtime)?;
        }
        let blob = Some(Blob { file, size });
        if part == "asset" {
            slot.asset = blob;
        } else {
            slot.meta = blob;
        }
    }

    // A member's source path is its GUID's pathname when the package names one.
    for (name, reason) in odd {
        let named = member_parts(&name).and_then(|(guid, part)| {
            let pathname = guids.get(guid)?.pathname.as_ref()?;
            match part {
                "asset" => Some(pathname.clone()),
                "asset.meta" => Some(format!("{pathname}.meta")),
                _ => None,
            }
        });
        job.out.skipped.push(SkippedEntry {
            source_path: source(named.as_deref().unwrap_or(&name)),
            reason,
        });
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
            Err(reason) => job.out.skipped.push(SkippedEntry {
                source_path: source(&pathname),
                reason,
            }),
        }
    }

    // Drop a top folder every file shares.
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
            let nested = File::open(&item.asset.file)?;
            unpack(
                job,
                nested,
                &source(&item.pathname),
                inner_subpath,
                &join(dest, &inner_dest),
            )
            .with_context(|| format!("unpacking the nested package {}", item.pathname))?;
            fs::remove_file(&item.asset.file)?;
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
        place(job, &item.asset, &rel)?;
        job.out.files.push(ExtractedFile {
            path: rel.clone(),
            source_path: source(&item.pathname),
            size: item.asset.size,
        });
        if let Some(meta) = item.meta {
            let rel = format!("{rel}.meta");
            place(job, &meta, &rel)?;
            job.out.files.push(ExtractedFile {
                path: rel,
                source_path: source(&format!("{}.meta", item.pathname)),
                size: meta.size,
            });
        }
    }
    Ok(())
}

/// Moves a staged entry to `rel` below the target, refusing to replace anything there.
fn place(job: &Job, blob: &Blob, rel: &str) -> Result<()> {
    if rel.split('/').next() == Some(job.scratch_name.as_str()) {
        bail!("{rel} would land in the extraction's scratch folder");
    }
    let to = job.target.join(rel);
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating the folder of {rel}"))?;
    }
    if to.symlink_metadata().is_ok() {
        bail!("{rel} would overwrite a file the package already wrote");
    }
    fs::rename(&blob.file, &to).with_context(|| format!("writing {rel}"))
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
fn clean(path: &str) -> std::result::Result<Vec<String>, SkipReason> {
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
fn below<'p>(path: &'p str, folder: &str) -> Option<&'p str> {
    match path.strip_prefix(folder)? {
        "" => Some(""),
        rest => rest.strip_prefix('/'),
    }
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}/{b}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pack-import")
            .join(name)
    }

    fn golden(name: &str) -> Vec<String> {
        fs::read_to_string(fixture("expected").join(name))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Every path below `root`, sorted, checking that each is a regular file or a folder with
    /// one link.
    fn on_disk(root: &Path) -> Vec<String> {
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
            tar.append_data(&mut header, name, *data).unwrap();
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
        let err = extract_unitypackage(&original, &tmp.path().join("u"), None).unwrap_err();
        assert!(err.to_string().contains("x.txt would overwrite"), "{err}");
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
