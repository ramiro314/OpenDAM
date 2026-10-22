//! The export cache: converted files kept in each library's `.opendam/export-cache/` (plan D3).
//! It is never pruned automatically in v1 (A9), so Settings offers "Clear export cache".

use std::path::{Path, PathBuf};

use crate::db::LibraryHandle;

/// The export cache folder of the library at `root`.
pub fn cache_dir(root: &Path) -> PathBuf {
    root.join(".opendam").join("export-cache")
}

/// Deletes every open library's export cache folder, skipping a library that has none. Returns
/// how many folders it deleted.
pub async fn clear(libraries: &[LibraryHandle]) -> Result<usize, String> {
    let mut cleared = 0;
    for library in libraries {
        let dir = cache_dir(&library.root_path);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => cleared += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "Could not clear the export cache of the library {}: {e}",
                    library.name
                ))
            }
        }
    }
    tracing::info!("Cleared the export cache of {cleared} libraries");
    Ok(cleared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memory_library_pool;

    #[tokio::test]
    async fn clearing_deletes_each_cache_and_skips_libraries_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut libraries = Vec::new();
        for id in ["cached", "uncached"] {
            let root = dir.path().join(id);
            std::fs::create_dir_all(root.join(".opendam/thumbs")).unwrap();
            libraries.push(LibraryHandle {
                id: id.into(),
                name: id.into(),
                root_path: root,
                pool: memory_library_pool().await,
            });
        }
        let cached = cache_dir(&libraries[0].root_path);
        std::fs::create_dir_all(cached.join("ab")).unwrap();
        std::fs::write(cached.join("ab/model.fbx"), "fbx").unwrap();

        assert_eq!(clear(&libraries).await, Ok(1));

        assert!(!cached.exists());
        assert!(libraries[0].root_path.join(".opendam/thumbs").is_dir());
        assert_eq!(clear(&libraries).await, Ok(0), "absent caches are fine");
    }
}
