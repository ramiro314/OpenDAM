//! The export cache: converted files kept in each library's `.opendam/export-cache/` (plan D3,
//! C11), so re-exporting an unchanged domain skips the work and writes byte-identical files. It
//! is never pruned automatically in v1 (A9), so Settings offers "Clear export cache".

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::db::LibraryHandle;
use crate::export::blender::{self, Job, Normalized};
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

/// The cache key of a model that `job` normalizes from a source whose sha256 is `source` (C11):
/// the sha256 of the canonical JSON of the source's sha256, every setting the script reads
/// besides the FBX (the kind, the origin's facing, your quarter turns, the collision patterns and
/// the added material's name), the tool, `BLENDER_SERIES` rather than the exact version, so a
/// lookup needs no Blender, and `NORMALIZE_VERSION`. Neither the file's path nor its name is in
/// it, so renaming a file keeps its entry.
pub fn model_key(source: &str, job: &Job) -> String {
    let material = json!({
        "inputs": [source],
        "settings": {
            "kind": job.kind,
            "originFacing": job.origin_facing,
            "facingQuarterTurns": job.facing_quarter_turns,
            "patterns": job.patterns,
            "addedMaterial": job.added_material,
        },
        "tool": "Blender",
        "toolVersion": blender::BLENDER_SERIES,
        "version": blender::NORMALIZE_VERSION,
    });
    hashes::sha256_bytes(canonical(&material).as_bytes())
}

/// Where the model of key `key` is cached in the library at `root`: a Blender job writes its FBX
/// there, and `save_model` then records its result beside it.
pub fn model_fbx(root: &Path, key: &str) -> PathBuf {
    cache_dir(root).join("models").join(format!("{key}.fbx"))
}

/// Where the Game tab's "Show as exported" keeps the GLB Blender writes of the cached model of
/// key `key` in the library at `root`, beside its entry (C13). It is only ever viewed.
pub fn model_view(root: &Path, key: &str) -> PathBuf {
    cache_dir(root).join("models").join(format!("{key}.glb"))
}

fn model_result(root: &Path, key: &str) -> PathBuf {
    cache_dir(root).join("models").join(format!("{key}.json"))
}

/// A normalized model in the export cache: its FBX, the FBX's sha256 and Blender's result,
/// which records the exact version that wrote it (C11).
#[derive(Debug, Clone, PartialEq)]
pub struct CachedModel {
    pub fbx: PathBuf,
    pub sha256: String,
    pub normalized: Normalized,
}

/// The model of key `key` the cache in the library at `root` holds, or `None` on a miss: no
/// result, a result that can't be read, or an FBX that is missing or isn't the file the result
/// was saved with, as one a crash cut off. Its FBX is hashed to tell, which the manifest needs
/// anyway.
pub fn cached_model(root: &Path, key: &str) -> Option<CachedModel> {
    let text = std::fs::read(model_result(root, key)).ok()?;
    let entry: Value = serde_json::from_slice(&text).ok()?;
    let sha256 = entry.get("sha256")?.as_str()?.to_string();
    let normalized: Normalized = serde_json::from_value(entry.get("result")?.clone()).ok()?;
    let fbx = model_fbx(root, key);
    (hashes::sha256_file(&fbx).ok()? == sha256).then_some(CachedModel {
        fbx,
        sha256,
        normalized,
    })
}

/// Records `normalized`, the result of the job that wrote the FBX of key `key` in the library at
/// `root`, completing the entry. The FBX is synced to disk first, then the result, which names
/// the FBX's sha256, is written beside its place, synced and renamed in, so an entry is a hit
/// only once both are whole.
pub fn save_model(root: &Path, key: &str, normalized: Normalized) -> Result<CachedModel, String> {
    let fbx = model_fbx(root, key);
    let save = || -> std::io::Result<CachedModel> {
        std::fs::File::open(&fbx)?.sync_all()?;
        let sha256 = hashes::sha256_file(&fbx)?;
        let entry = json!({ "sha256": sha256, "result": normalized });
        let bytes = serde_json::to_vec_pretty(&entry).map_err(std::io::Error::other)?;
        write_synced(&model_result(root, key), &bytes)?;
        Ok(CachedModel {
            fbx: fbx.clone(),
            sha256,
            normalized,
        })
    };
    save().map_err(|e| format!("its normalized FBX can't be saved in the export cache: {e}"))
}

/// Writes `bytes` beside `path`, syncs them to disk and renames them into place, so neither a
/// process crash nor an OS crash leaves a partial file at `path`.
fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().expect("a cache file has a folder");
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .expect("a cache file has a name")
        .to_string_lossy();
    let partial = dir.join(format!(".{name}.{}.partial", uuid::Uuid::new_v4()));
    let saved = std::fs::File::create(&partial)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&partial, path));
    if saved.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    saved?;
    // The rename is durable once the folder is: best effort, since some file systems can't
    // sync a folder
    if let Ok(folder) = std::fs::File::open(dir) {
        let _ = folder.sync_all();
    }
    Ok(())
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
    write_synced(&path, &bytes)
        .map_err(|e| format!("its converted file can't be saved in the export cache: {e}"))?;
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
    use crate::export::collision::{Patterns, PATTERNS};
    use crate::export::packs::Origin;

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

    fn lamp_job() -> Job {
        Job {
            asset: "Dungeon Props/Models/SM_Lamp.fbx".into(),
            kind: blender::JobKind::Model,
            input: "/lib/Dungeon Props/Models/SM_Lamp.fbx".into(),
            output: "/lib/.opendam/export-cache/models/k.fbx".into(),
            origin_facing: Origin::Unreal,
            facing_quarter_turns: 1,
            patterns: &PATTERNS,
            added_material: "OpenDAMDefault".into(),
        }
    }

    /// Collision patterns other than `PATTERNS`
    static OTHER_PATTERNS: Patterns = Patterns {
        prefixes: &["UCX_", "UBX_"],
        suffixes: &["_Collision"],
    };

    #[test]
    fn a_model_key_covers_the_source_and_every_setting_the_script_reads_but_no_path() {
        let a = "a".repeat(64);
        let key = model_key(&a, &lamp_job());
        assert_eq!(key.len(), 64);
        // Renaming or moving the file, or caching it elsewhere, keeps its entry
        let moved = Job {
            asset: "Props/SM_LampRenamed.fbx".into(),
            input: "/elsewhere/SM_LampRenamed.fbx".into(),
            output: "/elsewhere/out.fbx".into(),
            ..lamp_job()
        };
        assert_eq!(model_key(&a, &moved), key);

        let changed = [
            model_key(&"b".repeat(64), &lamp_job()),
            model_key(
                &a,
                &Job {
                    origin_facing: Origin::Generic,
                    ..lamp_job()
                },
            ),
            model_key(
                &a,
                &Job {
                    facing_quarter_turns: 2,
                    ..lamp_job()
                },
            ),
            model_key(
                &a,
                &Job {
                    added_material: "OpenDAMDefault2".into(),
                    ..lamp_job()
                },
            ),
            model_key(
                &a,
                &Job {
                    patterns: &OTHER_PATTERNS,
                    ..lamp_job()
                },
            ),
        ];
        for (i, other) in changed.iter().enumerate() {
            assert_ne!(*other, key, "change {i}");
        }
    }

    /// AC15's cache half: no model keeps its entry once the collision patterns change, so the
    /// next export re-runs every job.
    #[test]
    fn changing_the_collision_patterns_changes_every_model_key() {
        for (source, origin, turns, added) in [
            ("a", Origin::Generic, 0, "OpenDAMDefault"),
            ("b", Origin::Unreal, 3, "OpenDAMDefault2"),
            ("c", Origin::Unity, 1, "OpenDAMDefault"),
        ] {
            let source = source.repeat(64);
            let job = Job {
                origin_facing: origin,
                facing_quarter_turns: turns,
                added_material: added.into(),
                ..lamp_job()
            };
            let other = Job {
                patterns: &OTHER_PATTERNS,
                ..job.clone()
            };
            assert_ne!(model_key(&source, &job), model_key(&source, &other));
        }
    }

    #[test]
    fn a_saved_model_is_a_hit_until_its_fbx_or_result_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let normalized =
            blender::test_normalized(&["DefaultMaterial"], Some("OpenDAMDefault"), &[], &[]);
        assert_eq!(
            cached_model(root, "k"),
            None,
            "an empty cache holds nothing"
        );

        // A job writes the FBX, then the result completes the entry
        let fbx = model_fbx(root, "k");
        std::fs::create_dir_all(fbx.parent().unwrap()).unwrap();
        std::fs::write(&fbx, b"normalized FBX").unwrap();
        assert_eq!(cached_model(root, "k"), None, "an FBX alone is no entry");
        let saved = save_model(root, "k", normalized.clone()).unwrap();
        assert_eq!(
            saved,
            CachedModel {
                fbx: fbx.clone(),
                sha256: hashes::sha256_bytes(b"normalized FBX"),
                normalized,
            }
        );
        assert_eq!(cached_model(root, "k"), Some(saved.clone()));
        let entries = std::fs::read_dir(fbx.parent().unwrap()).unwrap().count();
        assert_eq!(entries, 2, "no partial file is left");

        // An FBX cut off or rewritten since is a miss, and so is a result that can't be read
        std::fs::write(&fbx, b"normalized").unwrap();
        assert_eq!(cached_model(root, "k"), None);
        std::fs::write(&fbx, b"normalized FBX").unwrap();
        assert_eq!(cached_model(root, "k"), Some(saved));
        let result = fbx.with_extension("json");
        let text = std::fs::read(&result).unwrap();
        std::fs::write(&result, &text[..text.len() / 2]).unwrap();
        assert_eq!(cached_model(root, "k"), None);
        std::fs::remove_file(&fbx).unwrap();
        assert!(save_model(root, "k", blender::test_normalized(&[], None, &[], &[])).is_err());
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
