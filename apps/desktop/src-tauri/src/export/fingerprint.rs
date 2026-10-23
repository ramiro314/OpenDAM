//! What a Check read (plan game-export C14), so Export can tell whether anything changed since:
//! the domain's members and names, the settings, the packs, and every file the Check opened or
//! looked for and found missing. Export recomputes it and swaps the bundle in only when the two
//! are equal.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// A file as a Check found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stamp {
    Missing,
    File {
        size: u64,
        mtime_ns: i128,
    },
    /// Something other than a file, such as a folder
    Other,
}

impl Stamp {
    /// The stamp of whatever is at `path`, following symlinks.
    pub fn of(path: &Path) -> Stamp {
        match std::fs::metadata(path) {
            Err(_) => Stamp::Missing,
            Ok(meta) if !meta.is_file() => Stamp::Other,
            Ok(meta) => Stamp::File {
                size: meta.len(),
                mtime_ns: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos() as i128),
            },
        }
    }

    pub fn is_file(self) -> bool {
        matches!(self, Stamp::File { .. })
    }
}

impl fmt::Display for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Stamp::Missing => write!(f, "missing"),
            Stamp::File { size, mtime_ns } => write!(f, "{size} bytes at {mtime_ns}"),
            Stamp::Other => write!(f, "not a file"),
        }
    }
}

/// The files of one library a Check stats, by their path relative to the library root, each
/// stamped when it is first looked at.
#[derive(Debug, Clone)]
pub struct Probes {
    root: PathBuf,
    pub seen: BTreeMap<String, Stamp>,
}

impl Probes {
    pub fn new(root: &Path) -> Self {
        Probes {
            root: root.to_path_buf(),
            seen: BTreeMap::new(),
        }
    }

    /// Stamps the file at `path` (relative to the library root) and records it.
    pub fn probe(&mut self, path: &str) -> Stamp {
        let stamp = Stamp::of(&self.root.join(path));
        self.seen.insert(path.to_string(), stamp);
        stamp
    }
}

/// Each input a Check read, by its scope (a library id, or `""` for the master DB and settings)
/// and a label that names it for people, holding its value as text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fingerprint(BTreeMap<(String, String), String>);

impl Fingerprint {
    pub fn put(&mut self, scope: &str, label: impl Into<String>, value: impl fmt::Display) {
        self.0
            .insert((scope.to_string(), label.into()), value.to_string());
    }

    /// The labels of the inputs that differ between `self` and `now`: changed, gone or new.
    pub fn changes(&self, now: &Fingerprint) -> Vec<String> {
        let mut changed: Vec<String> = Vec::new();
        for (key, value) in &self.0 {
            if now.0.get(key) != Some(value) {
                changed.push(key.1.clone());
            }
        }
        for key in now.0.keys() {
            if !self.0.contains_key(key) {
                changed.push(key.1.clone());
            }
        }
        changed.dedup();
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_name_changed_gone_and_new_inputs() {
        let mut before = Fingerprint::default();
        before.put("", "the domain's name", "Props");
        before.put("lib", "a.png", "1 bytes at 5");
        before.put("lib", "b.png", "missing");
        let mut after = before.clone();
        assert_eq!(before.changes(&after), Vec::<String>::new());

        after.put("", "the domain's name", "Loot");
        after.0.remove(&("lib".into(), "a.png".into()));
        after.put("lib", "c.png", "missing");
        assert_eq!(
            before.changes(&after),
            ["the domain's name", "a.png", "c.png"]
        );
    }

    #[test]
    fn probes_stamp_files_folders_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.png"), "abc").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut probes = Probes::new(dir.path());

        assert!(matches!(probes.probe("a.png"), Stamp::File { size: 3, .. }));
        assert_eq!(probes.probe("sub"), Stamp::Other);
        assert_eq!(probes.probe("gone.meta"), Stamp::Missing);
        assert_eq!(probes.seen.len(), 3);
    }
}
