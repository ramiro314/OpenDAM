//! The `extract_zip` step (plan pack-import, Steps): copies a zip original, or one sub-folder of
//! it, into a folder tree. It also finds a variant `.unitypackage` inside a zip for
//! `unitypackage::extract_variant`.
//!
//! The original is only read. Every file it writes is a regular file it creates and fills from an
//! entry's bytes, so no link is ever made. Unsafe entries (absolute or `..` names), symlinks,
//! devices, FIFOs and every other entry that isn't a regular file or a folder are skipped and
//! reported, as is a repeated path after the first (`P/a` again as `P/./a`). The zip reader
//! itself keeps one entry, the last, of an exactly repeated name, so that repeat goes unreported.
//! Folders are made as the files below them
//! need them. A file keeps its path in the zip, with no top folder dropped: a layout's wrappers
//! are the `flatten` step's to merge.
//!
//! With a sub-folder, only the files below it are written, relative to it, and an entry elsewhere
//! in the zip is neither written nor reported, unless its name is unsafe, which places it nowhere.
//!
//! `list_zip` lists what `extract_zip` would write from the zip's central directory, for an
//! import's scan (`super::scan`), and `index_zip` reads that directory once to list several
//! folders. Both decide what to write and skip through `ZipIndex`, as `extract_zip` does.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use zip::read::ZipFile;
use zip::{ExtraField, ZipArchive};

use super::unitypackage::{
    below, clean, clean_subpath, into_empty, ExtractedFile, Extraction, SkipReason, SkippedEntry,
    Taken,
};

/// Copies the zip `original` into `target`, which must be empty or missing, or with a `subpath`
/// (a folder in the zip), only the files below it. Each file's source path is its name in the
/// zip. A clash, a file that would land on another, fails the extraction. A failed extraction
/// leaves the target as it found it: empty, or missing.
pub fn extract_zip(original: &Path, target: &Path, subpath: Option<&str>) -> Result<Extraction> {
    let subpath = clean_subpath(subpath)?;
    into_empty(target, |target| {
        let mut zip = open(original)?;
        let mut out = Extraction::default();
        let index = ZipIndex::read(&mut zip)?;
        for wanted in index.wanted(subpath.as_deref(), &mut out)? {
            let (rel, name) = (wanted.rel, wanted.name);
            let mut entry = zip
                .by_index(wanted.index)
                .with_context(|| format!("reading {name} in the zip"))?;
            let to = target.join(&rel);
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating the folder of {rel}"))?;
            }
            if to.symlink_metadata().is_ok() {
                bail!("{rel} would overwrite a file the zip already wrote");
            }
            let mut file = File::create_new(&to).with_context(|| format!("writing {rel}"))?;
            let size = io::copy(&mut entry, &mut file)
                .with_context(|| format!("reading {name} in the zip"))?;
            if let Some(mtime) = mtime(&entry) {
                file.set_modified(mtime)?;
            }
            out.files.push(ExtractedFile {
                path: rel,
                source_path: vec![name],
                size,
            });
        }
        out.files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    })
}

/// Lists the files `extract_zip` would write, with their sizes and the entries it would skip,
/// from the zip's central directory alone: no entry is read and nothing is written. It fails
/// where the extraction would.
pub fn list_zip(original: &Path, subpath: Option<&str>) -> Result<Extraction> {
    index_zip(original)?.list(subpath)
}

/// Reads the zip `original`'s central directory once, for listing several of its folders
/// (`ZipIndex::list`) without reading it again.
pub fn index_zip(original: &Path) -> Result<ZipIndex> {
    ZipIndex::read(&mut open(original)?)
}

/// A zip's entries as its central directory records them. It decides, in one place, which
/// files an extraction of the zip or of one of its folders writes and which entries it skips.
pub struct ZipIndex {
    entries: Vec<Entry>,
}

struct Entry {
    index: usize,
    name: String,
    /// The name's clean components, or why the name is unsafe.
    parts: std::result::Result<Vec<String>, SkipReason>,
    kind: Kind,
    size: u64,
}

/// A file of the zip the extraction writes.
struct Wanted {
    index: usize,
    /// Its path below the target.
    rel: String,
    /// Its name in the zip.
    name: String,
    /// Its size as the central directory records it.
    size: u64,
}

impl ZipIndex {
    fn read(zip: &mut ZipArchive<File>) -> Result<Self> {
        let mut entries = Vec::with_capacity(zip.len());
        for index in 0..zip.len() {
            let entry = zip.by_index_raw(index).context("reading the zip")?;
            let name = name(&entry);
            entries.push(Entry {
                index,
                parts: clean(&name),
                kind: kind(&entry),
                size: entry.size(),
                name,
            });
        }
        Ok(ZipIndex { entries })
    }

    /// Lists what `extract_zip` would write with this sub-folder, as `list_zip` does.
    pub fn list(&self, subpath: Option<&str>) -> Result<Extraction> {
        let subpath = clean_subpath(subpath)?;
        let mut out = Extraction::default();
        let mut taken = Taken::default();
        for wanted in self.wanted(subpath.as_deref(), &mut out)? {
            taken.claim(&wanted.rel, "zip")?;
            out.files.push(ExtractedFile {
                path: wanted.rel,
                source_path: vec![wanted.name],
                size: wanted.size,
            });
        }
        out.files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// The entry index of every file `list(None)` lists, by its name there (its source path).
    /// The names are unique: the listing keeps one file per clean path.
    pub(super) fn file_entries(&self) -> std::collections::HashMap<String, usize> {
        let mut skipped = Extraction::default();
        self.wanted(None, &mut skipped)
            .unwrap_or_default()
            .into_iter()
            .map(|wanted| (wanted.name, wanted.index))
            .collect()
    }

    /// The files below the clean `subpath`, else all of them, in the zip's order. Adds the
    /// entries it skips to `out`.
    fn wanted(&self, subpath: Option<&str>, out: &mut Extraction) -> Result<Vec<Wanted>> {
        let mut seen = HashSet::new();
        let mut wanted = Vec::new();
        for entry in &self.entries {
            let mut skip = |reason| {
                out.skipped.push(SkippedEntry {
                    source_path: vec![entry.name.clone()],
                    reason,
                })
            };
            let parts = match &entry.parts {
                Ok(parts) => parts,
                Err(reason) => {
                    skip(*reason);
                    continue;
                }
            };
            if entry.kind == Kind::Folder {
                continue;
            }
            if parts.is_empty() {
                skip(SkipReason::NoName);
                continue;
            }
            let path = parts.join("/");
            let rel = match subpath {
                None => path.clone(),
                Some(sub) => match below(&path, sub) {
                    Some(rel) if !rel.is_empty() => rel.to_string(),
                    _ => continue,
                },
            };
            if let Kind::Skip(reason) = entry.kind {
                skip(reason);
                continue;
            }
            if !seen.insert(path) {
                skip(SkipReason::Duplicate);
                continue;
            }
            wanted.push(Wanted {
                index: entry.index,
                rel,
                name: entry.name.clone(),
                size: entry.size,
            });
        }
        if let (Some(subpath), true) = (subpath, wanted.is_empty()) {
            bail!("the zip holds no file under {subpath}");
        }
        Ok(wanted)
    }
}

pub(super) fn open(original: &Path) -> Result<ZipArchive<File>> {
    let file = File::open(original).with_context(|| format!("opening {}", original.display()))?;
    ZipArchive::new(file).with_context(|| format!("reading the zip {}", original.display()))
}

/// Copies the entry at `index` in `zip`, one of the files `ZipIndex::file_entries` names, to a
/// new file at `to`, with the entry's mtime, as the extraction writes each file, and returns its
/// size.
pub(super) fn copy_entry(zip: &mut ZipArchive<File>, index: usize, to: &Path) -> Result<u64> {
    let mut entry = zip
        .by_index(index)
        .with_context(|| format!("reading entry {index} of the zip"))?;
    let name = self::name(&entry);
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating the folder of {name}"))?;
    }
    let mut file = File::create_new(to).with_context(|| format!("writing {}", to.display()))?;
    let size =
        io::copy(&mut entry, &mut file).with_context(|| format!("reading {name} in the zip"))?;
    if let Some(mtime) = mtime(&entry) {
        file.set_modified(mtime)?;
    }
    Ok(size)
}

/// The index and name of the regular file at `variant` in `zip`, compared on clean paths.
pub(super) fn variant(zip: &mut ZipArchive<File>, variant: &str) -> Result<(usize, String)> {
    let want = match clean(variant) {
        Ok(parts) if !parts.is_empty() => parts.join("/"),
        _ => bail!("invalid variant {variant:?}"),
    };
    for index in 0..zip.len() {
        let entry = zip.by_index_raw(index).context("reading the zip")?;
        let name = name(&entry);
        if clean(&name).map(|parts| parts.join("/")).ok().as_ref() != Some(&want) {
            continue;
        }
        match kind(&entry) {
            Kind::Folder => continue,
            Kind::File => return Ok((index, name)),
            Kind::Skip(_) => bail!("{name} in the zip isn't a regular file"),
        }
    }
    bail!("the zip holds no {want}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Folder,
    Skip(SkipReason),
}

/// What an entry is, from its Unix file type when the zip records one. An entry with none, or
/// from a DOS or Windows zip tool, is a folder when its name ends in `/`, else a file.
fn kind<R: Read>(entry: &ZipFile<'_, R>) -> Kind {
    match entry.unix_mode().map(|mode| mode & 0o170000) {
        Some(0o120000) => Kind::Skip(SkipReason::Symlink),
        Some(0o040000) => Kind::Folder,
        None | Some(0) | Some(0o100000) if entry.is_dir() => Kind::Folder,
        None | Some(0) | Some(0o100000) => Kind::File,
        Some(_) => Kind::Skip(SkipReason::Special),
    }
}

/// An entry's name, decoded as the zip says (UTF-8 or CP437).
fn name<R: Read>(entry: &ZipFile<'_, R>) -> String {
    match entry.name() {
        Ok(name) => name.into_owned(),
        Err(_) => String::from_utf8_lossy(entry.name_raw()).into_owned(),
    }
}

/// An entry's mtime: its extended timestamp when it has one, else its DOS time, which has no
/// time zone and is read as UTC so that every machine gets the same mtime.
fn mtime<R: Read>(entry: &ZipFile<'_, R>) -> Option<SystemTime> {
    let extended = entry.extra_data_fields().find_map(|field| match field {
        ExtraField::ExtendedTimestamp(stamp) => stamp.mod_time().map(i64::from),
        _ => None,
    });
    let secs = match extended {
        Some(secs) => secs,
        None => {
            let t = entry.last_modified()?;
            chrono::NaiveDate::from_ymd_opt(t.year().into(), t.month().into(), t.day().into())?
                .and_hms_opt(t.hour().into(), t.minute().into(), t.second().into())?
                .and_utc()
                .timestamp()
        }
    };
    UNIX_EPOCH.checked_add(Duration::from_secs(u64::try_from(secs).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::unitypackage::extract_variant;
    use crate::import::unitypackage::tests::{fixture, on_disk};
    use std::io::Write;

    const CAR_KIT: &str = "Kenney/3D assets/Fixture Car Kit";

    fn paths(extraction: &Extraction) -> Vec<&str> {
        extraction.files.iter().map(|f| f.path.as_str()).collect()
    }

    fn skipped(extraction: &Extraction) -> Vec<(&str, SkipReason)> {
        extraction
            .skipped
            .iter()
            .map(|s| {
                assert_eq!(s.source_path.len(), 1);
                (s.source_path[0].as_str(), s.reason)
            })
            .collect()
    }

    #[test]
    fn a_kenney_pack_is_copied_by_its_sub_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("Fixture Car Kit");
        let extraction = extract_zip(&fixture("kenney.zip"), &target, Some(CAR_KIT)).unwrap();

        let expected = ["License.txt", "Models/car.glb", "Preview.png"];
        assert_eq!(paths(&extraction), expected);
        assert_eq!(on_disk(&target), expected);
        assert!(extraction.skipped.is_empty(), "{:?}", extraction.skipped);
        for file in &extraction.files {
            assert_eq!(file.source_path, [format!("{CAR_KIT}/{}", file.path)]);
            let on_disk = target.join(&file.path);
            assert_eq!(fs::metadata(&on_disk).unwrap().len(), file.size);
            let mtime = fs::metadata(&on_disk).unwrap().modified().unwrap();
            assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        }
        let license = fs::read_to_string(target.join("License.txt")).unwrap();
        assert!(license.contains("Fixture Car Kit (1.0)"), "{license}");
        assert!(license.contains("Creative Commons Zero, CC0"), "{license}");
        assert!(fs::read(target.join("Models/car.glb"))
            .unwrap()
            .starts_with(b"glTF"));
    }

    #[test]
    fn the_whole_zip_keeps_its_top_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let extraction = extract_zip(&fixture("kenney.zip"), tmp.path(), None).unwrap();
        assert_eq!(
            paths(&extraction),
            [
                "Kenney/2D assets/Fixture Tiles/License.txt",
                "Kenney/2D assets/Fixture Tiles/Tiles/tile_0000.png",
                "Kenney/2D assets/Fixture Tiles/Tiles/tile_0001.png",
                "Kenney/3D assets/Fixture Car Kit/License.txt",
                "Kenney/3D assets/Fixture Car Kit/Models/car.glb",
                "Kenney/3D assets/Fixture Car Kit/Preview.png",
                "Kenney/Archive/Fixture Old Kit/License.txt",
                "Kenney/Archive/Fixture Old Kit/old.png",
            ]
        );
        assert_eq!(on_disk(tmp.path()), paths(&extraction));
    }

    #[test]
    fn unsafe_and_special_entries_are_skipped_and_reported() {
        let absolute = Path::new("/tmp/opendam-pack-import-absolute-zip.txt");
        let _ = fs::remove_file(absolute);
        let unsafe_names = [
            ("../escape.txt", SkipReason::Escapes),
            (absolute.to_str().unwrap(), SkipReason::Absolute),
        ];
        let in_esc = [
            ("Esc/./b.txt", SkipReason::Duplicate),
            ("Esc/link.txt", SkipReason::Symlink),
            ("Esc/fifo", SkipReason::Special),
        ];
        type Case<'a> = (Option<&'a str>, &'a [&'a str], Vec<(&'a str, SkipReason)>);
        let cases: [Case; 3] = [
            (
                None,
                &["Esc/Keep/a.txt", "Esc/b.txt"],
                [&unsafe_names[..], &in_esc[..]].concat(),
            ),
            (
                Some("Esc"),
                &["Keep/a.txt", "b.txt"],
                [&unsafe_names[..], &in_esc[..]].concat(),
            ),
            // An entry outside the sub-folder isn't the pack's, but an unsafe name is nowhere.
            (Some("Esc/Keep"), &["a.txt"], unsafe_names.to_vec()),
        ];
        for (subpath, expected, reported) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let target = tmp.path().join("Pack");
            let extraction = extract_zip(&fixture("escape.zip"), &target, subpath).unwrap();
            assert_eq!(paths(&extraction), expected, "{subpath:?}");
            // `on_disk` also checks that every file is regular, with one link.
            assert_eq!(on_disk(&target), expected, "{subpath:?}");
            assert_eq!(skipped(&extraction), reported, "{subpath:?}");
            // Nothing lands outside the target, and the first `b.txt` wins.
            assert_eq!(on_disk(tmp.path()).len(), expected.len());
            assert!(!tmp.path().join("escape.txt").exists());
            assert!(!absolute.exists());
            if subpath == Some("Esc") {
                assert_eq!(fs::read(target.join("b.txt")).unwrap(), b"first b\n");
            }
        }
    }

    #[test]
    fn a_variant_is_extracted_from_inside_the_zip() {
        let fab = fixture("fab-unity.zip");
        let expected = [
            "Art/Materials/M_Stone.mat",
            "Art/Materials/M_Stone.mat.meta",
            "Art/Textures/T_Stone.png",
            "Art/Textures/T_Stone.png.meta",
            "Demo/Demo.unity",
            "Demo/Demo.unity.meta",
            "Pipeline.txt",
            "Pipeline.txt.meta",
            "Readme.txt",
            "Readme.txt.meta",
        ];
        for pipeline in ["URP", "Built-In"] {
            let variant = format!("Crypt/Crypt_{pipeline}.unitypackage");
            let tmp = tempfile::tempdir().unwrap();
            let extraction = extract_variant(&fab, &variant, tmp.path(), None).unwrap();
            assert_eq!(paths(&extraction), expected, "{pipeline}");
            assert_eq!(on_disk(tmp.path()), expected, "{pipeline}");
            assert_eq!(
                fs::read_to_string(tmp.path().join("Pipeline.txt")).unwrap(),
                format!("{pipeline}\n")
            );
            // The variant's path in the zip starts each source path.
            let readme = extraction.files.iter().find(|f| f.path == "Readme.txt");
            assert_eq!(
                readme.unwrap().source_path,
                [variant.as_str(), "Assets/Crypt/Readme.txt"]
            );
        }

        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("none");
        let err =
            extract_variant(&fab, "Crypt/Crypt_HDRP.unitypackage", &missing, None).unwrap_err();
        assert!(
            err.to_string().contains("holds no Crypt/Crypt_HDRP"),
            "{err}"
        );
        assert!(!missing.exists());
    }

    #[test]
    fn a_pipeline_folder_is_extracted_from_a_package_in_the_zip() {
        let folders = fixture("fab-unity-folders.zip");
        let package = "Crypt/Crypt.unitypackage";
        for (pipeline, wrapper) in [("URP", "URP_Crypt"), ("Built-In", "Built-In_Crypt")] {
            let tmp = tempfile::tempdir().unwrap();
            let extraction =
                extract_variant(&folders, package, tmp.path(), Some(pipeline)).unwrap();
            let files = paths(&extraction);
            assert_eq!(files.len(), 8, "{files:?}");
            assert!(files.iter().all(|f| f.starts_with(&format!("{wrapper}/"))));
            assert_eq!(
                fs::read_to_string(tmp.path().join(wrapper).join("Pipeline.txt")).unwrap(),
                format!("{pipeline}\n")
            );
        }
        let tmp = tempfile::tempdir().unwrap();
        let err = extract_variant(&folders, package, tmp.path(), Some("HDRP")).unwrap_err();
        assert!(err.to_string().contains("no file under HDRP"), "{err}");
    }

    #[test]
    fn the_original_is_untouched() {
        let stamp = |path: &Path| {
            (
                fs::read(path).unwrap(),
                fs::metadata(path).unwrap().modified().unwrap(),
            )
        };
        let runs: [(&str, Option<&str>, Option<&str>); 5] = [
            ("kenney.zip", None, None),
            ("kenney.zip", None, Some(CAR_KIT)),
            ("escape.zip", None, None),
            ("fab-unity.zip", Some("Crypt/Crypt_URP.unitypackage"), None),
            (
                "fab-unity-folders.zip",
                Some("Crypt/Crypt.unitypackage"),
                Some("URP"),
            ),
        ];
        for (name, variant, subpath) in runs {
            let original = fixture(name);
            let before = stamp(&original);
            let tmp = tempfile::tempdir().unwrap();
            match variant {
                Some(variant) => extract_variant(&original, variant, tmp.path(), subpath),
                None => extract_zip(&original, tmp.path(), subpath),
            }
            .unwrap();
            assert!(stamp(&original) == before, "{name} changed");
        }
    }

    fn stored() -> zip::write::SimpleFileOptions {
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)
    }

    /// A zip of stored `(name, bytes)` files.
    fn zip_of(path: &Path, entries: &[(&str, &[u8])]) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, data) in entries {
            zip.start_file(*name, stored()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn clashes_and_empty_sub_folders_fail_and_leave_the_target_as_found() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("clash.zip");
        // A file, then a file below a folder of the same name.
        zip_of(&original, &[("P/a", b"a"), ("P/a/b", b"b")]);
        let missing = tmp.path().join("t");
        let err = extract_zip(&original, &missing, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("creating the folder of P/a/b"),
            "{err:#}"
        );
        assert!(!missing.exists());

        // The other order.
        zip_of(&original, &[("P/a/b", b"b"), ("P/a", b"a")]);
        let empty = tmp.path().join("u");
        fs::create_dir(&empty).unwrap();
        let err = extract_zip(&original, &empty, None).unwrap_err();
        assert!(err.to_string().contains("P/a would overwrite"), "{err}");
        assert_eq!(fs::read_dir(&empty).unwrap().count(), 0);

        let err = extract_zip(&original, &empty, Some("Q")).unwrap_err();
        assert!(err.to_string().contains("no file under Q"), "{err}");
        let err = extract_zip(&original, &empty, Some("../P")).unwrap_err();
        assert!(err.to_string().contains("invalid sub-folder"), "{err}");
        assert_eq!(fs::read_dir(&empty).unwrap().count(), 0);
    }

    #[test]
    fn a_variant_that_is_a_link_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let original = tmp.path().join("link.zip");
        let mut zip = zip::ZipWriter::new(File::create(&original).unwrap());
        zip.add_symlink("V.unitypackage", "/etc/passwd", stored())
            .unwrap();
        zip.finish().unwrap();
        let err =
            extract_variant(&original, "V.unitypackage", &tmp.path().join("t"), None).unwrap_err();
        assert!(err.to_string().contains("isn't a regular file"), "{err}");
    }
}
