//! The export cache: converted files kept in each library's `.opendam/export-cache/` (plan D3,
//! C11), so re-exporting an unchanged domain skips the work and writes byte-identical files. It
//! is never pruned automatically in v1 (A9), so Settings offers "Clear export cache".

use std::io::{Read, Seek, SeekFrom, Write};
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

/// The first bytes of every PNG file.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// The last bytes of every PNG file: its empty `IEND` chunk and that chunk's CRC.
const PNG_END: [u8; 12] = [0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82];

/// Whether the file at `path` starts with the PNG signature and ends with the `IEND` chunk, so an
/// entry an OS crash left empty, or one cut off anywhere, isn't taken for a hit. Damage inside an
/// otherwise whole file isn't detected: that would mean decoding every entry on every Check.
fn is_whole_png(path: &Path) -> bool {
    let read = || -> std::io::Result<bool> {
        let mut file = std::fs::File::open(path)?;
        let length = file.metadata()?.len();
        if length < (PNG_SIGNATURE.len() + PNG_END.len()) as u64 {
            return Ok(false);
        }
        let mut head = [0; 8];
        file.read_exact(&mut head)?;
        let mut tail = [0; 12];
        file.seek(SeekFrom::End(-(PNG_END.len() as i64)))?;
        file.read_exact(&mut tail)?;
        Ok(head == PNG_SIGNATURE && tail == PNG_END)
    };
    read().unwrap_or(false)
}

/// The cached texture of key `key` in the library at `root`, made by `make` and saved first when
/// the cache doesn't hold it. A file is written beside its place, synced to disk and renamed
/// into it, so neither a process crash nor an OS crash leaves a partial entry under the key, and
/// an entry that isn't a whole PNG from signature to `IEND` is made again.
pub fn texture(
    root: &Path,
    key: &str,
    make: impl FnOnce() -> Result<Vec<u8>, String>,
) -> Result<PathBuf, String> {
    let dir = cache_dir(root).join("textures");
    let path = dir.join(format!("{key}.png"));
    if is_whole_png(&path) {
        return Ok(path);
    }
    let bytes = make()?;
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let partial = dir.join(format!(".{key}.{}.partial", uuid::Uuid::new_v4()));
        let saved = std::fs::File::create(&partial)
            .and_then(|mut file| {
                file.write_all(&bytes)?;
                file.sync_all()
            })
            .and_then(|()| std::fs::rename(&partial, &path));
        if saved.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        saved?;
        // The rename is durable once the folder is: best effort, since some file systems can't
        // sync a folder
        if let Ok(folder) = std::fs::File::open(&dir) {
            let _ = folder.sync_all();
        }
        Ok(())
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
        let png = [&PNG_SIGNATURE[..], b"pixels", &PNG_END[..]].concat();
        let make = || {
            made.set(made.get() + 1);
            Ok(png.clone())
        };
        let path = texture(dir.path(), "k", make).unwrap();
        assert_eq!(path, cache_dir(dir.path()).join("textures/k.png"));
        assert_eq!(std::fs::read(&path).unwrap(), png);
        let again = texture(dir.path(), "k", || panic!("a hit does no work")).unwrap();
        assert_eq!(again, path);
        assert_eq!(made.get(), 1);
        let entries = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(entries, 1, "no partial file is left");

        let failed = texture(dir.path(), "other", || Err("broken".into()));
        assert_eq!(failed, Err("broken".into()));
        assert!(!cache_dir(dir.path()).join("textures/other.png").exists());
    }

    #[test]
    fn an_empty_or_truncated_entry_is_made_again() {
        let dir = tempfile::tempdir().unwrap();
        let png = [&PNG_SIGNATURE[..], b"pixels", &PNG_END[..]].concat();
        let textures = cache_dir(dir.path()).join("textures");
        std::fs::create_dir_all(&textures).unwrap();
        // What an OS crash can leave after a rename whose data never reached the disk
        let cut = &png[..png.len() - 1];
        let headless = &png[1..];
        for (key, left) in [
            ("empty", &b""[..]),
            ("short", &PNG_SIGNATURE[..4]),
            ("cut", cut),
            ("headless", headless),
        ] {
            std::fs::write(textures.join(format!("{key}.png")), left).unwrap();
            let path = texture(dir.path(), key, || Ok(png.clone())).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), png, "{key}");
            let hit = texture(dir.path(), key, || panic!("a whole entry is a hit")).unwrap();
            assert_eq!(hit, path);
        }
        let entries = std::fs::read_dir(&textures).unwrap().count();
        assert_eq!(entries, 4, "no partial file is left");
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
