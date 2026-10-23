//! The export cache: converted files kept in each library's `.opendam/export-cache/` (plan D3,
//! C11), so re-exporting an unchanged domain skips the work and writes byte-identical files. It
//! is never pruned automatically in v1 (A9), so Settings offers "Clear export cache".

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::db::LibraryHandle;
use crate::export::{hashes, textures};

/// The export cache folder of the library at `root`.
pub fn cache_dir(root: &Path) -> PathBuf {
    root.join(".opendam").join("export-cache")
}

/// `value` as JSON with every object's keys sorted, whatever order it was built in.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::from(k.as_str()), canonical(&map[k])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", items.join(","))
        }
        other => other.to_string(),
    }
}

/// The cache key of a texture made from inputs whose sha256s are `inputs`, in order, by
/// `steps`: the sha256 of the canonical JSON of the inputs, the steps (each with its tool, tool
/// version and settings) and `textures::VERSION`.
pub fn texture_key(inputs: &[String], steps: &[Value]) -> String {
    let material = json!({
        "inputs": inputs,
        "steps": steps,
        "tool": "OpenDAM",
        "toolVersion": textures::tool_version(),
        "version": textures::VERSION,
    });
    hashes::sha256_bytes(canonical(&material).as_bytes())
}

/// The cached texture of key `key` in the library at `root`, made by `make` and saved first when
/// the cache doesn't hold it. A file is written beside its place and renamed into it, so a
/// crash never leaves a partial entry.
pub fn texture(
    root: &Path,
    key: &str,
    make: impl FnOnce() -> Result<Vec<u8>, String>,
) -> Result<PathBuf, String> {
    let dir = cache_dir(root).join("textures");
    let path = dir.join(format!("{key}.png"));
    if path.is_file() {
        return Ok(path);
    }
    let bytes = make()?;
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let partial = dir.join(format!(".{key}.{}.partial", uuid::Uuid::new_v4()));
        std::fs::write(&partial, &bytes)?;
        std::fs::rename(&partial, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&partial);
        })
    };
    write().map_err(|e| format!("its converted file can't be saved in the export cache: {e}"))?;
    Ok(path)
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

    #[test]
    fn a_texture_key_covers_inputs_in_order_steps_and_the_version() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let step = |role: &str| json!({"tool": "OpenDAM", "toolVersion": textures::tool_version(), "by": "merge-metallic-smoothness", "from": "png", "settings": {"layout": "Roughness", "role": role}});
        let key = texture_key(&[a.clone(), b.clone()], &[step("MetallicSmoothness")]);
        assert_eq!(key.len(), 64);
        assert_eq!(
            key,
            texture_key(&[a.clone(), b.clone()], &[step("MetallicSmoothness")]),
            "the same inputs and steps give the same key"
        );
        assert_ne!(
            key,
            texture_key(&[b.clone(), a.clone()], &[step("MetallicSmoothness")])
        );
        assert_ne!(
            key,
            texture_key(std::slice::from_ref(&a), &[step("MetallicSmoothness")])
        );
        assert_ne!(key, texture_key(&[a, b], &[step("Occlusion")]));
    }

    #[test]
    fn canonical_json_sorts_keys_at_every_level() {
        let mut map = serde_json::Map::new();
        map.insert("z".into(), json!(1));
        map.insert("a".into(), json!([{"y": true, "b": "x"}]));
        assert_eq!(
            canonical(&Value::Object(map)),
            r#"{"a":[{"b":"x","y":true}],"z":1}"#
        );
    }

    #[test]
    fn a_cached_texture_is_made_once_and_then_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Ok(b"png".to_vec())
        };
        let path = texture(dir.path(), "k", make).unwrap();
        assert_eq!(path, cache_dir(dir.path()).join("textures/k.png"));
        assert_eq!(std::fs::read(&path).unwrap(), b"png");
        let again = texture(dir.path(), "k", || panic!("a hit does no work")).unwrap();
        assert_eq!(again, path);
        assert_eq!(made.get(), 1);
        let entries = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(entries, 1, "no partial file is left");

        let failed = texture(dir.path(), "other", || Err("broken".into()));
        assert_eq!(failed, Err("broken".into()));
        assert!(!cache_dir(dir.path()).join("textures/other.png").exists());
    }

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
