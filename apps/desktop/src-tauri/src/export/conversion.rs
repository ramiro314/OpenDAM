//! `conversion.json` files (plan game-export C10). A file's conversion record is the nearest
//! `conversion.json` from its folder up to its library root: a deeper one replaces a shallower
//! one. Pack origin inference (C6) uses the same walk, and so does the guard that refuses a
//! texture under a `conversion.json` until conversion chains are built (plan "Build order").

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::export::contract;

pub const FILE_NAME: &str = "conversion.json";

/// The `conversion.json` nearest to a folder.
#[derive(Debug, Clone, PartialEq)]
pub struct Nearest {
    /// Relative to the library root, with `/` separators
    pub path: String,
    /// Its steps, or why it is invalid: unreadable, not JSON, or not a `$defs/conversionFile`
    pub steps: Result<Vec<Value>, Vec<String>>,
}

impl Nearest {
    /// Whether it is valid and has a step converted from `from`, such as `uasset`.
    pub fn has_step_from(&self, from: &str) -> bool {
        self.steps
            .as_ref()
            .is_ok_and(|steps| steps.iter().any(|s| s["from"] == from))
    }
}

/// Walks folders of one library to their nearest `conversion.json`, reading each folder's file at
/// most once, so asking for every file of a pack costs one probe per folder.
pub struct Walker {
    root: PathBuf,
    folders: HashMap<String, Option<Arc<Nearest>>>,
}

impl Walker {
    pub fn new(root: &Path) -> Self {
        Walker {
            root: root.to_path_buf(),
            folders: HashMap::new(),
        }
    }

    /// The nearest `conversion.json` at or above `dir` (relative to the library root, `""` for
    /// the root), or `None` when no folder up to the root has one.
    pub fn nearest(&mut self, dir: &str) -> Option<Arc<Nearest>> {
        let dir = dir.trim_matches('/');
        if let Some(found) = self.folders.get(dir) {
            return found.clone();
        }
        let path = if dir.is_empty() {
            FILE_NAME.to_string()
        } else {
            format!("{dir}/{FILE_NAME}")
        };
        let absolute = self.root.join(&path);
        let found = if absolute.is_file() {
            Some(Arc::new(Nearest {
                steps: read(&absolute),
                path,
            }))
        } else if dir.is_empty() {
            None
        } else {
            let parent = dir.rsplit_once('/').map_or("", |(parent, _)| parent);
            self.nearest(parent)
        };
        self.folders.insert(dir.to_string(), found.clone());
        found
    }

    /// The nearest `conversion.json` of the file at `file_path` (relative to the library root).
    pub fn nearest_for_file(&mut self, file_path: &str) -> Option<Arc<Nearest>> {
        let dir = file_path.rsplit_once('/').map_or("", |(dir, _)| dir);
        self.nearest(dir)
    }
}

fn read(path: &Path) -> Result<Vec<Value>, Vec<String>> {
    let bytes = std::fs::read(path).map_err(|e| vec![format!("can't be read: {e}")])?;
    let file: Value =
        serde_json::from_slice(&bytes).map_err(|e| vec![format!("is not JSON: {e}")])?;
    contract::validate_conversion_file(&file)?;
    Ok(file["steps"].as_array().cloned().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(root: &Path, path: &str, value: &Value) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value.to_string()).unwrap();
    }

    fn conversion(from: &str) -> Value {
        json!({"contractVersion": 1, "steps": [{"tool": "Exporter", "toolVersion": "1", "from": from}]})
    }

    #[test]
    fn the_deepest_file_up_to_the_root_wins() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "conversion.json", &conversion("blend"));
        write(root, "Packs/Ruins/conversion.json", &conversion("uasset"));
        std::fs::create_dir_all(root.join("Packs/Ruins/Textures/4K")).unwrap();
        let mut walker = Walker::new(root);

        let deep = walker
            .nearest_for_file("Packs/Ruins/Textures/4K/T_Wall_D.png")
            .unwrap();
        assert_eq!(deep.path, "Packs/Ruins/conversion.json");
        assert!(deep.has_step_from("uasset"));
        assert_eq!(
            walker.nearest("Packs/Other").unwrap().path,
            "conversion.json"
        );
        assert_eq!(
            walker.nearest_for_file("rock.png").unwrap().path,
            "conversion.json"
        );
    }

    #[test]
    fn no_file_up_to_the_root_is_none() {
        let dir = tempfile::tempdir().unwrap();
        // Above the library root doesn't count
        write(dir.path(), "conversion.json", &conversion("uasset"));
        let root = dir.path().join("library");
        std::fs::create_dir_all(root.join("a/b")).unwrap();

        assert_eq!(Walker::new(&root).nearest("a/b"), None);
    }

    #[test]
    fn an_invalid_file_is_found_with_its_errors() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "a/conversion.json",
            &json!({"contractVersion": 1, "steps": []}),
        );
        std::fs::write(dir.path().join("conversion.json"), "{not json").unwrap();
        let mut walker = Walker::new(dir.path());

        let invalid = walker.nearest("a").unwrap();
        assert!(invalid.steps.is_err());
        assert!(!invalid.has_step_from("uasset"));
        let broken = walker.nearest("").unwrap();
        assert!(broken.steps.as_ref().unwrap_err()[0].starts_with("is not JSON"));
    }
}
