//! The `unity_glb` step (plan pack-import, Unity conversion): a textured `X.glb` beside each
//! model of a staged pack, made by the embedded `unity_glb.py` (a port of `unity_gltf.py`) in
//! Blender, under a memory cap (`capped`), and cached by everything Blender reads.
//!
//! - **Which models.** One GLB per model stem in a folder, from the FBX when the folder holds
//!   both `X.fbx` and `X.obj`, as the format group would pick it, and none where the folder
//!   already ships `X.glb` (`models`). The GLB is named after the model's final name and keeps
//!   its source path, recorded as `generated`.
//! - **A work copy.** Blender reads a copy of the staged pack's inputs (Unity files, models,
//!   `.mtl` files and textures) in a work folder of the cache, never the staging folder, and the
//!   folder is deleted when the step ends.
//! - **Binary Unity files.** A `.mat`, `.prefab` or `.unity` that Unity 4 or 5 wrote in its
//!   binary serialization is rewritten as text in the work copy only, by the preview worker's
//!   `unity_binary_to_text` (UnityPy), so the script reads it and the library keeps its bytes.
//!   Only a file with the binary serialization's header counts (`is_serialized_file`), so another
//!   program's `.mat` is copied as it is. One the worker can't rewrite fails the step, naming it.
//! - **The cache.** Each GLB is kept at `<cache>/glb/<key>.glb`, keyed by the original's sha256,
//!   the model's source path, the options, `UNITY_GLB_VERSION`, Blender's version and the
//!   **inputs hash** (`inputs_hash`), and for a pack holding binary Unity files, the converter's
//!   version (`converter_version`), so a rebuild that only moved files between folders runs no
//!   Blender at all, and one that dropped, renamed or changed an input converts again.
//! - **A model left with no mesh.** Its empty GLB is still written and cached, so the step reads
//!   each GLB it places, new or cached, and warns of an empty one on every run, unless the
//!   model's file name is a collider's (`is_collider`). An animation clip holds no mesh either,
//!   but its GLB isn't empty (`is_empty_glb`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use super::build::{Progress, ProgressSink, RunControl, Stop};
use super::unitypackage::{SkipReason, SkippedEntry};
use crate::capped;
use crate::export::blender::{self, Blender};
use crate::export::hashes::sha256_bytes;
use crate::preview::WorkerRequest;
use crate::structure::split_lod;

/// The script's version, which every GLB's cache key holds. The script declares it as
/// `VERSION`, which a test keeps equal: bump both when the GLBs it writes change.
pub const UNITY_GLB_VERSION: &str = "opendam-unity-glb/5";

/// The version of the preview engine's `unity_text.py`, which writes binary Unity files as text.
/// It declares it as `VERSION`, which a test keeps equal: bump both when the text it writes
/// changes.
pub const UNITY_TEXT_VERSION: &str = "opendam-unity-text/1";

/// The preview engine's lockfile, for the UnityPy version the text is written with.
const ENGINE_LOCK: &str = include_str!("../../../../../packages/preview-engine/uv.lock");

/// What a binary Unity file's text depends on besides its bytes: `UNITY_TEXT_VERSION` and the
/// UnityPy version the engine's lockfile pins, which the installed engine runs.
pub fn converter_version() -> String {
    let unitypy = ENGINE_LOCK
        .split("[[package]]")
        .find(|p| p.contains("\nname = \"unitypy\"\n"))
        .and_then(|p| p.lines().find_map(|l| l.strip_prefix("version = \"")))
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or("unknown");
    format!("{UNITY_TEXT_VERSION} UnityPy {unitypy}")
}

/// The conversion script, embedded so the installed app needs no file beside it.
const SCRIPT: &str = include_str!("unity_glb.py");

/// Blender's memory cap when Settings sets none (D20).
pub const DEFAULT_MEMORY_MAX: &str = "12G";

const MODEL_EXTENSIONS: [&str; 2] = ["fbx", "obj"];
/// The files the script loads as textures. It reads them from its job (`imageExtensions`), so
/// this is the one list of them.
const IMAGE_EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "tga", "tif", "tiff", "psd", "bmp"];
/// The other files Blender reads besides the models: Unity's `.meta`, `.mat`, `.prefab`, `.unity`
/// and `.shadergraph`, and an OBJ's `.mtl`. A test keeps every extension the script names among
/// these.
const UNITY_EXTENSIONS: [&str; 6] = ["meta", "mat", "prefab", "unity", "shadergraph", "mtl"];
/// The Unity files the script reads as text that Unity 4 and 5 could also write in their binary
/// serialization.
const SERIALIZED_EXTENSIONS: [&str; 3] = ["mat", "prefab", "unity"];
/// A collision mesh's name starts with one of these or ends with one of `COLLIDER_SUFFIXES`,
/// lowercased: the terms of the script's `COLLISION_RE`, which a test keeps equal.
const COLLIDER_PREFIXES: [&str; 1] = ["ucx_"];
const COLLIDER_SUFFIXES: [&str; 5] = [
    "_convexhull",
    "_convexhulls",
    "_collider",
    "_colliders",
    "_collision",
];

/// The step's options. `force` converts every model again, whatever the cache holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    #[serde(default = "default_max_texture")]
    pub max_texture: u32,
    #[serde(default)]
    pub force: bool,
}

fn default_max_texture() -> u32 {
    1024
}

impl Options {
    pub fn parse(options: &serde_json::Value) -> Result<Self, String> {
        let options = match options {
            serde_json::Value::Null => json!({}),
            other => other.clone(),
        };
        let parsed: Options = serde_json::from_value(options)
            .map_err(|e| format!("The unity_glb step's options are invalid: {e}"))?;
        if !(16..=16384).contains(&parsed.max_texture) {
            return Err("The unity_glb step's max_texture must be 16 to 16384 px".into());
        }
        Ok(parsed)
    }

    /// The step's default options, as an import records them.
    pub fn default_json() -> serde_json::Value {
        json!({ "max_texture": default_max_texture() })
    }
}

/// What the step runs with, read from Settings for each run.
#[derive(Debug, Clone)]
pub struct Tools {
    /// `export.blenderPath`, unset for `blender` on `PATH` (`blender::gate`).
    pub blender_path: Option<String>,
    /// `import.blenderMemoryMax`.
    pub memory_max: String,
    /// The import's cache folder: `glb/` for the GLBs, `work/` for the work copies.
    pub cache_dir: PathBuf,
    /// `import.assimpPath`, else `assimp` on `PATH`.
    pub assimp: String,
    /// The systemd tools Blender's scope runs through.
    pub systemd: capped::Systemd,
    /// The preview queue's worker, which rewrites binary Unity files as text.
    pub worker: mpsc::Sender<WorkerRequest>,
}

impl Tools {
    pub async fn load(
        master: &sqlx::Pool<sqlx::Sqlite>,
        cache_dir: PathBuf,
        worker: mpsc::Sender<WorkerRequest>,
    ) -> Self {
        let blender_path = match crate::export::settings::load(master).await {
            Ok(settings) => settings.blender_path,
            Err(e) => {
                tracing::warn!("Could not read Blender's path from Settings: {e}");
                None
            }
        };
        let settings = super::settings::load(master).await.unwrap_or_else(|e| {
            tracing::warn!("Could not read the import settings: {e}");
            Default::default()
        });
        Tools {
            blender_path,
            memory_max: settings.blender_memory_max(),
            cache_dir,
            assimp: settings.assimp(),
            systemd: capped::Systemd::default(),
            worker,
        }
    }

    /// Why the step can't run here, `None` when the Blender it would run is accepted.
    pub async fn unavailable(&self) -> Option<String> {
        blender::gate(self.blender_path.as_deref())
            .await
            .err()
            .map(needs_blender)
    }
}

fn needs_blender(why: String) -> String {
    format!(
        "Converting to GLB needs Blender {}: {why}",
        blender::BLENDER_SERIES
    )
}

/// `${XDG_CACHE_HOME:-~/.cache}/opendam/import`.
pub fn default_cache_dir() -> PathBuf {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| var("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("opendam")
        .join("import")
}

/// A staged file as the step reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    /// Below the pack root, `/`-separated.
    pub path: String,
    pub source_path: Vec<String>,
    pub sha256: String,
}

/// A model the step gives a GLB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// The model, below the pack root.
    pub path: String,
    pub source_path: Vec<String>,
    /// The GLB, beside it.
    pub glb: String,
}

fn extension(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// A file name without its last extension, as Python's `Path.stem`.
fn stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// The models of `files` that get a GLB, sorted by path: per folder and stem (ignoring case),
/// the FBX, else the OBJ, the first by path among several, unless the folder ships a GLB of
/// that stem.
pub fn models(files: &[Staged]) -> Vec<Model> {
    let shipped: HashSet<(&str, String)> = files
        .iter()
        .filter(|f| extension(&f.path).as_deref() == Some("glb"))
        .map(|f| {
            let (folder, name) = split(&f.path);
            (folder, stem(name).to_lowercase())
        })
        .collect();
    let mut chosen: BTreeMap<(&str, String), &Staged> = BTreeMap::new();
    let rank = |f: &Staged| {
        let ext = extension(&f.path).unwrap_or_default();
        MODEL_EXTENSIONS.iter().position(|e| *e == ext)
    };
    for file in files {
        let Some(this) = rank(file) else { continue };
        let (folder, name) = split(&file.path);
        let key = (folder, stem(name).to_lowercase());
        if shipped.contains(&key) {
            continue;
        }
        chosen
            .entry(key)
            .and_modify(|best| {
                if (this, &file.path) < (rank(best).unwrap_or(usize::MAX), &best.path) {
                    *best = file;
                }
            })
            .or_insert(file);
    }
    let mut models: Vec<Model> = chosen
        .into_values()
        .map(|f| {
            let (folder, name) = split(&f.path);
            let glb = format!("{}.glb", stem(name));
            Model {
                path: f.path.clone(),
                source_path: f.source_path.clone(),
                glb: if folder.is_empty() {
                    glb
                } else {
                    format!("{folder}/{glb}")
                },
            }
        })
        .collect();
    models.sort_by(|a, b| a.path.cmp(&b.path));
    models
}

/// Whether Blender reads `path`: a model, a texture or another of the files the script reads.
pub fn is_input(path: &str) -> bool {
    extension(path).is_some_and(|ext| {
        [&MODEL_EXTENSIONS[..], &IMAGE_EXTENSIONS, &UNITY_EXTENSIONS]
            .iter()
            .any(|set| set.contains(&ext.as_str()))
    })
}

/// Whether the file at `path` is named as a collider, with or without `_LOD<n>` as the script
/// reads a mesh's name: its meshes are all the script drops, so its empty GLB is the known
/// collider gap, not something to warn of.
fn is_collider(path: &str) -> bool {
    let name = stem(split(path).1).to_lowercase();
    let name = split_lod(&name).map_or(name.as_str(), |(base, _)| base);
    COLLIDER_PREFIXES.iter().any(|p| name.starts_with(p))
        || COLLIDER_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Whether the GLB at `path` holds neither a mesh nor an animation (a clip such as `Hero@Run.fbx`
/// holds no mesh), from its JSON chunk, the first, alone: the rest of a GLB can run to hundreds of
/// MB.
fn is_empty_glb(path: &Path) -> Result<bool, String> {
    use std::io::Read;
    #[derive(Deserialize)]
    struct Gltf {
        #[serde(default)]
        meshes: Vec<serde::de::IgnoredAny>,
        #[serde(default)]
        animations: Vec<serde::de::IgnoredAny>,
    }
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut head = [0u8; 20];
    file.read_exact(&mut head).map_err(|e| e.to_string())?;
    if &head[..4] != b"glTF" || &head[16..] != b"JSON" {
        return Err("it doesn't start with a JSON chunk".into());
    }
    let len = u32::from_le_bytes(head[12..16].try_into().unwrap());
    let mut json = Vec::new();
    file.take(len as u64)
        .read_to_end(&mut json)
        .map_err(|e| e.to_string())?;
    let gltf: Gltf = serde_json::from_slice(&json).map_err(|e| e.to_string())?;
    Ok(gltf.meshes.is_empty() && gltf.animations.is_empty())
}

/// An input as the inputs hash reads it: a staged file Blender reads, and for a `.meta`, the
/// GUID it declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub path: String,
    pub source_path: Vec<String>,
    pub sha256: String,
    pub guid: Option<String>,
}

/// Lowercased with everything but ASCII letters and digits dropped, as the script's `alnum`.
fn alnum(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

/// The hash of everything the script reads, which every GLB's key holds:
///
/// - each input by its source path, its final file name and its sha256, sorted;
/// - and for every set of inputs the script tells apart only by the order of their paths, their
///   source paths in that order (by final path, lowercased, as the script sorts): inputs whose
///   names match once lowercased and stripped to letters and digits (its `images` and `by_name`
///   maps, the first of each name winning), `.meta` files declaring the same GUID (its GUID
///   index), and the prefabs and the scenes (its prefab table, the first naming a mesh winning).
///
/// So moving files between folders keeps the hash unless it changes which of such a set comes
/// first, and dropping, renaming or changing an input always changes it.
pub fn inputs_hash(inputs: &[Input]) -> String {
    let source = |i: &Input| crate::import::records::source_json(&i.source_path);
    let mut files: Vec<(String, &str, &str)> = inputs
        .iter()
        .map(|i| (source(i), split(&i.path).1, i.sha256.as_str()))
        .collect();
    files.sort();
    let mut sets: BTreeMap<String, Vec<&Input>> = BTreeMap::new();
    for input in inputs {
        let name = split(&input.path).1;
        sets.entry(format!("name:{}", alnum(stem(name))))
            .or_default()
            .push(input);
        if let Some(guid) = &input.guid {
            sets.entry(format!("guid:{guid}")).or_default().push(input);
        }
        match extension(name).as_deref() {
            Some("prefab") => sets.entry("prefabs".into()).or_default().push(input),
            Some("unity") => sets.entry("scenes".into()).or_default().push(input),
            _ => {}
        }
    }
    let orders: Vec<(String, Vec<String>)> = sets
        .into_iter()
        .filter(|(_, set)| set.len() > 1)
        .map(|(key, mut set)| {
            set.sort_by(|a, b| {
                (a.path.to_lowercase(), &a.path).cmp(&(b.path.to_lowercase(), &b.path))
            });
            (key, set.into_iter().map(source).collect())
        })
        .collect();
    sha256_bytes(
        json!({ "files": files, "orders": orders })
            .to_string()
            .as_bytes(),
    )
}

/// A GLB's cache key. `converter` is the converter's version (`converter_version`) when the
/// pack holds binary Unity files, whose text Blender reads, and `None` otherwise, so a pack
/// without them keeps its key whatever the converter does.
pub fn glb_key(
    original_sha256: &str,
    model: &Model,
    options: &Options,
    blender_version: &str,
    inputs_hash: &str,
    converter: Option<&str>,
) -> String {
    let mut key = json!({
        "original": original_sha256,
        "model": model.source_path,
        "maxTexture": options.max_texture,
        "version": UNITY_GLB_VERSION,
        "blender": blender_version,
        "inputs": inputs_hash,
    });
    if let Some(converter) = converter {
        key["converter"] = json!(converter);
    }
    sha256_bytes(key.to_string().as_bytes())
}

/// The GUID a `.meta` declares in its first 2000 bytes, as the script reads it.
fn meta_guid(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut head = Vec::with_capacity(2000);
    std::fs::File::open(path)
        .ok()?
        .take(2000)
        .read_to_end(&mut head)
        .ok()?;
    let text = String::from_utf8_lossy(&head);
    let at = text.find("guid: ")? + "guid: ".len();
    let guid = text.get(at..at + 32)?;
    guid.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        .then(|| guid.to_string())
}

/// Whether `head`, the start of a file of `len` bytes, is the header of Unity's binary
/// serialization: big-endian, the format version at byte 8, and the file's own size at byte 4
/// (versions before 22) or as a 64-bit number at byte 24 (22 and later).
pub fn is_serialized_file(head: &[u8], len: u64) -> bool {
    let u32_at = |at: usize| {
        head.get(at..at + 4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()) as u64)
    };
    let u64_at = |at: usize| {
        head.get(at..at + 8)
            .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
    };
    match u32_at(8) {
        Some(1..22) => u32_at(4) == Some(len) && u32_at(12).is_some_and(|data| data <= len),
        Some(22..100) => u64_at(24) == Some(len),
        _ => false,
    }
}

/// Whether the file at `path` is a `.mat`, `.prefab` or `.unity` in Unity's binary
/// serialization (`is_serialized_file`). Unity's text format starts with `%YAML` instead, and
/// another program's file of those extensions with neither is copied as it is.
fn is_binary_unity(path: &Path) -> bool {
    use std::io::Read;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if !extension(name).is_some_and(|ext| SERIALIZED_EXTENSIONS.contains(&ext.as_str())) {
        return false;
    }
    let mut head = Vec::with_capacity(32);
    let read = std::fs::File::open(path).and_then(|f| {
        let len = f.metadata()?.len();
        f.take(32).read_to_end(&mut head)?;
        Ok(len)
    });
    read.is_ok_and(|len| is_serialized_file(&head, len))
}

/// One run of the step over a staged pack.
pub struct Conversion<'a> {
    pub staging: &'a Path,
    /// Every staged file.
    pub files: &'a [Staged],
    pub original_sha256: &'a str,
    pub options: &'a Options,
    pub tools: &'a Tools,
    pub control: Arc<RunControl>,
    pub progress: ProgressSink,
}

/// The job the script reads.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Job<'a> {
    pack: &'a Path,
    scratch: PathBuf,
    max_texture: u32,
    assimp: &'a str,
    /// `IMAGE_EXTENSIONS`, as `.png`.
    image_extensions: Vec<String>,
    models: Vec<JobModel>,
}

#[derive(Serialize)]
struct JobModel {
    model: String,
    output: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScriptResult {
    version: String,
    models: Vec<ModelResult>,
}

#[derive(Debug, Deserialize)]
struct ModelResult {
    model: String,
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

/// Deletes a work folder when the step ends, however it ends.
struct WorkFolder(PathBuf);

impl Drop for WorkFolder {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("Could not delete the work folder {}: {e}", self.0.display());
            }
        }
    }
}

impl Conversion<'_> {
    fn cancelled(&self) -> Result<(), Stop> {
        if self.control.cancelled() {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Writes each model's GLB into the staging folder, from the cache or from Blender, and
    /// returns the models that got one, adding to `warnings` each that holds no mesh. A model
    /// Blender fails on fails the step, naming it.
    pub async fn run(&self, warnings: &mut Vec<SkippedEntry>) -> Result<Vec<Model>, Stop> {
        let models = models(self.files);
        if models.is_empty() {
            return Ok(models);
        }
        (self.progress)(Progress {
            step: "converting",
            done: 0,
            total: models.len() as u64,
        });
        let blender = blender::gate(self.tools.blender_path.as_deref())
            .await
            .map_err(needs_blender)?;
        let (staging, files) = (self.staging.to_path_buf(), self.files.to_vec());
        let (inputs, has_binary) = blocking(move || {
            let inputs: Vec<Input> = files
                .into_iter()
                .filter(|f| is_input(&f.path))
                .map(|f| Input {
                    guid: (extension(&f.path).as_deref() == Some("meta"))
                        .then(|| meta_guid(&staging.join(&f.path)))
                        .flatten(),
                    path: f.path,
                    source_path: f.source_path,
                    sha256: f.sha256,
                })
                .collect();
            let has_binary = inputs
                .iter()
                .any(|i| is_binary_unity(&staging.join(&i.path)));
            Ok((inputs, has_binary))
        })
        .await?;
        let inputs_hash = inputs_hash(&inputs);
        let converter = has_binary.then(converter_version);
        let glb_dir = self.tools.cache_dir.join("glb");
        let keyed: Vec<(Model, PathBuf)> = models
            .iter()
            .map(|m| {
                let key = glb_key(
                    self.original_sha256,
                    m,
                    self.options,
                    &blender.version,
                    &inputs_hash,
                    converter.as_deref(),
                );
                (m.clone(), glb_dir.join(format!("{key}.glb")))
            })
            .collect();
        let uncached: Vec<&(Model, PathBuf)> = keyed
            .iter()
            .filter(|(_, cached)| self.options.force || !cached.is_file())
            .collect();
        if !uncached.is_empty() {
            self.convert(&blender, &inputs, &uncached, models.len())
                .await?;
        }
        self.cancelled()?;

        let (staging, placed) = (self.staging.to_path_buf(), keyed.clone());
        let empty = blocking(move || {
            let mut empty = Vec::new();
            for (model, cached) in &placed {
                let glb = staging.join(&model.glb);
                std::fs::copy(cached, &glb)
                    .map_err(|e| format!("Could not write {}: {e}", model.glb))?;
                match is_empty_glb(&glb) {
                    Ok(true) if !is_collider(&model.path) => empty.push(SkippedEntry {
                        source_path: model.source_path.clone(),
                        reason: SkipReason::NoMesh,
                    }),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("Could not read {}: {e}", model.glb),
                }
            }
            Ok(empty)
        })
        .await?;
        warnings.extend(empty);
        // Cached GLBs print nothing, and a Blender line may not have come.
        (self.progress)(Progress {
            step: "converting",
            done: models.len() as u64,
            total: models.len() as u64,
        });
        Ok(models)
    }

    /// Converts `uncached` in one Blender process over a work copy of `inputs`, and puts each
    /// GLB in the cache.
    async fn convert(
        &self,
        blender: &Blender,
        inputs: &[Input],
        uncached: &[&(Model, PathBuf)],
        total: usize,
    ) -> Result<(), Stop> {
        let work = WorkFolder(
            self.tools
                .cache_dir
                .join("work")
                .join(uuid::Uuid::new_v4().to_string()),
        );
        let pack = work.0.join("pack");
        let out = work.0.join("out");
        let (staging, copied, folders) = (
            self.staging.to_path_buf(),
            inputs.iter().map(|i| i.path.clone()).collect::<Vec<_>>(),
            (pack.clone(), out.clone()),
        );
        let control = self.control.clone();
        let binary = blocking(move || {
            std::fs::create_dir_all(&folders.1)
                .map_err(|e| format!("Could not make the conversion's work folder: {e}"))?;
            let mut binary = Vec::new();
            for path in copied {
                if control.cancelled() {
                    return Ok(binary);
                }
                let to = folders.0.join(&path);
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("Could not make the conversion's work folder: {e}"))?;
                }
                std::fs::copy(staging.join(&path), &to)
                    .map_err(|e| format!("Could not copy {path} to the work folder: {e}"))?;
                if is_binary_unity(&to) {
                    binary.push(path);
                }
            }
            Ok(binary)
        })
        .await?;
        self.cancelled()?;
        self.binary_to_text(&work.0, &pack, &binary).await?;

        let job = Job {
            pack: &pack,
            scratch: work.0.join("scratch"),
            max_texture: self.options.max_texture,
            assimp: &self.tools.assimp,
            image_extensions: IMAGE_EXTENSIONS.iter().map(|e| format!(".{e}")).collect(),
            models: uncached
                .iter()
                .enumerate()
                .map(|(n, (m, _))| JobModel {
                    model: m.path.clone(),
                    output: out.join(format!("{n}.glb")),
                })
                .collect(),
        };
        let script = work.0.join("unity_glb.py");
        let job_file = work.0.join("job.json");
        let result_file = work.0.join("result.json");
        let job_json = serde_json::to_vec(&job).map_err(|e| e.to_string())?;
        let written = match tokio::fs::write(&script, SCRIPT).await {
            Ok(()) => tokio::fs::write(&job_file, job_json).await,
            error => error,
        };
        written.map_err(|e| format!("Could not write Blender's job: {e}"))?;

        let args = [
            "-b".as_ref(),
            "--factory-startup".as_ref(),
            "--python-exit-code".as_ref(),
            "1".as_ref(),
            "-P".as_ref(),
            script.as_os_str(),
            "--".as_ref(),
            job_file.as_os_str(),
            result_file.as_os_str(),
        ];
        let cached_already = total - uncached.len();
        let ran = self
            .run_blender(blender, &args, &work.0, cached_already, total)
            .await?;
        // A crash, and a clean exit with no result file, are told apart by `crashed`.
        let text = match tokio::fs::read_to_string(&result_file).await {
            Ok(text) if ran.status.success() => text,
            _ => return Err(Stop::Failed(converting(blender::crashed(&ran)))),
        };
        let result: ScriptResult = serde_json::from_str(&text)
            .map_err(|e| converting(format!("Blender's conversion result can't be read: {e}")))?;
        if result.version != UNITY_GLB_VERSION {
            return Err(Stop::Failed(converting(format!(
                "the script reported version {}, not {UNITY_GLB_VERSION}",
                result.version
            ))));
        }
        let by_model: HashMap<&str, &ModelResult> = result
            .models
            .iter()
            .map(|r| (r.model.as_str(), r))
            .collect();
        for (n, (model, cached)) in uncached.iter().enumerate() {
            match by_model.get(model.path.as_str()) {
                Some(r) if r.ok => {}
                Some(r) => {
                    return Err(Stop::Failed(format!(
                        "Converting {} to GLB failed: {}",
                        model.path,
                        r.error.as_deref().unwrap_or("Blender gave no reason")
                    )))
                }
                None => {
                    return Err(Stop::Failed(converting(format!(
                        "Blender reported nothing for {}",
                        model.path
                    ))))
                }
            }
            publish(&out.join(format!("{n}.glb")), cached, "Blender")
                .await
                .map_err(|e| format!("Converting {} to GLB failed: {e}", model.path))?;
        }
        Ok(())
    }

    /// Rewrites each of `binary`, binary Unity files of the work copy at `pack`, as text, through
    /// the preview worker. Only the work copy changes: the staged files keep their bytes.
    async fn binary_to_text(
        &self,
        work: &Path,
        pack: &Path,
        binary: &[String],
    ) -> Result<(), Stop> {
        for (n, path) in binary.iter().enumerate() {
            self.cancelled()?;
            let failed =
                |why: String| Stop::Failed(format!("Reading {path} as text failed: {why}"));
            let source = pack.join(path);
            let output = work.join("text").join(format!("{n}.txt"));
            let (reply, answer) = oneshot::channel();
            let request = json!({
                "id": format!("unity_binary_to_text-{}", uuid::Uuid::new_v4()),
                "method": "unity_binary_to_text",
                "params": { "source": source, "output": output },
            });
            self.tools
                .worker
                .send(WorkerRequest { request, reply })
                .await
                .map_err(|_| failed("the preview engine isn't running".into()))?;
            let reply = answer
                .await
                .map_err(|_| failed("the preview engine stopped before it answered".into()))?
                .map_err(failed)?;
            if let Some(error) = reply.get("error").and_then(|e| e.as_str()) {
                return Err(failed(error.into()));
            }
            let result = &reply["result"];
            if result["ok"] != true {
                let why = result["error"]
                    .as_str()
                    .unwrap_or("the preview engine gave no reason");
                return Err(failed(why.into()));
            }
            tokio::fs::rename(&output, &source)
                .await
                .map_err(|e| failed(format!("its text can't be read: {e}")))?;
        }
        self.cancelled()
    }

    /// Runs Blender with `args` in `dir` under the memory cap, counting the models it reports
    /// for the progress, and stops it on Cancel.
    async fn run_blender(
        &self,
        blender: &Blender,
        args: &[&std::ffi::OsStr],
        dir: &Path,
        done: usize,
        total: usize,
    ) -> Result<std::process::Output, Stop> {
        let mut capped = capped::capped_command_with(
            &self.tools.systemd,
            &blender.program,
            args,
            &self.tools.memory_max,
        )
        .await;
        capped.command.current_dir(dir);
        let mut converted = done;
        // Uncapped, `capped_command_with` has warned.
        let output = capped
            .watch(
                |line| {
                    if line.starts_with(b"[unity-glb] model ") {
                        converted += 1;
                        (self.progress)(Progress {
                            step: "converting",
                            done: converted.min(total) as u64,
                            total: total as u64,
                        });
                    }
                },
                || self.control.cancelled(),
            )
            .await
            .map_err(|e| converting(format!("Could not run Blender: {e}")))?
            .ok_or(Stop::Cancelled)?;
        if let Some(killed) = capped.killed_by_cap(output.status) {
            return Err(Stop::Failed(converting(format!("Blender was {killed}"))));
        }
        Ok(output)
    }
}

fn converting(why: String) -> String {
    format!("Converting to GLB failed: {why}")
}

/// Puts the GLB at `made`, which the tool `who` wrote, in the cache at `cached`, copied beside it
/// and renamed, so the cache only ever holds whole GLBs.
pub(super) async fn publish(made: &Path, cached: &Path, who: &str) -> Result<(), String> {
    let mut head = [0u8; 4];
    let read = async {
        use tokio::io::AsyncReadExt;
        tokio::fs::File::open(made)
            .await?
            .read_exact(&mut head)
            .await
    };
    if read.await.is_err() || &head != b"glTF" {
        return Err(format!("{who} wrote no GLB"));
    }
    let dir = cached.parent().expect("in the cache folder");
    let partial = dir.join(format!(".{}.partial", uuid::Uuid::new_v4()));
    let written = async {
        tokio::fs::create_dir_all(dir).await?;
        tokio::fs::copy(made, &partial).await?;
        tokio::fs::rename(&partial, cached).await
    }
    .await;
    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(format!("Could not write {}: {e}", cached.display()));
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, Stop> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("The conversion's worker stopped: {e}"))?
        .map_err(Stop::Failed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn staged(path: &str) -> Staged {
        Staged {
            path: path.into(),
            source_path: vec![format!("Assets/Pack/{path}")],
            sha256: format!("sha of {path}"),
        }
    }

    #[test]
    fn the_script_declares_the_version_rust_records() {
        let declared = SCRIPT
            .lines()
            .find_map(|l| l.strip_prefix("VERSION = \""))
            .and_then(|rest| rest.strip_suffix('"'));
        assert_eq!(declared, Some(UNITY_GLB_VERSION));
    }

    /// Every extension the script names, in a glob or a suffix check, is one Rust copies into the
    /// work copy, and the textures it loads are the ones its job names.
    #[test]
    fn the_script_reads_only_files_rust_copies() {
        assert!(
            !SCRIPT.contains("IMAGE_EXTENSIONS"),
            "the script takes the image extensions from its job"
        );
        assert!(SCRIPT.contains(r#"set(job["imageExtensions"])"#));
        let copied: Vec<&str> =
            [&MODEL_EXTENSIONS[..], &IMAGE_EXTENSIONS, &UNITY_EXTENSIONS].concat();
        let named: Vec<&str> = SCRIPT
            .match_indices("\"*.")
            .chain(SCRIPT.match_indices("\"."))
            .filter_map(|(at, quote)| {
                let rest = &SCRIPT[at + quote.len()..];
                let ext = &rest[..rest.find('"')?];
                (!ext.is_empty() && ext.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(ext)
            })
            .collect();
        for ext in [
            "meta",
            "prefab",
            "unity",
            "mat",
            "shadergraph",
            "mtl",
            "fbx",
            "jpg",
            "jpeg",
        ] {
            assert!(named.contains(&ext), "the scan finds .{ext}: {named:?}");
        }
        for ext in &named {
            assert!(
                copied.contains(ext),
                "the script reads .{ext}, which isn't copied"
            );
        }
    }

    /// Every term of the script's `COLLISION_RE` is one of Rust's, and the other way round.
    #[test]
    fn rust_keeps_the_terms_of_the_script_s_collision_re() {
        let regex = SCRIPT
            .split_once("COLLISION_RE = re.compile(\n    r\"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .expect("the script declares COLLISION_RE")
            .0;
        let terms: Vec<&str> = regex.split('|').collect();
        let prefixes: Vec<&str> = terms.iter().filter_map(|t| t.strip_prefix('^')).collect();
        let suffixes: Vec<&str> = terms.iter().filter_map(|t| t.strip_suffix('$')).collect();
        assert_eq!(prefixes, COLLIDER_PREFIXES);
        assert_eq!(suffixes, COLLIDER_SUFFIXES);
        assert_eq!(terms.len(), prefixes.len() + suffixes.len(), "{regex}");
        for term in terms {
            assert!(
                term.trim_matches(['^', '$'])
                    .bytes()
                    .all(|b| b == b'_' || b.is_ascii_lowercase()),
                "a term of plain letters: {term}"
            );
        }
    }

    #[test]
    fn a_collider_s_file_name_matches_as_the_script_matches_a_mesh_s() {
        for path in [
            "UCX_Rock.obj",
            "Props/ucx_rock.fbx",
            "Rock_ConvexHull.fbx",
            "Rock_ConvexHulls.fbx",
            "rock_collider.obj",
            "Rock_COLLIDERS.obj",
            "Rock_Collision.fbx",
            "Rock_Collider_LOD1.obj",
            "Rock_Collider_LOD0.fbx",
            "UCX_Rock_LOD2.fbx",
        ] {
            assert!(is_collider(path), "{path}");
        }
        for path in [
            "Rock.obj",
            "SM_UCX_Rock.obj",
            "Rock_LOD1.obj",
            "Rock_Collider_LODs.obj",
            "Rock_Collisions.obj",
            "rock_collider/Rock.obj",
            "Rock_ConvexHull.Old.fbx",
        ] {
            assert!(!is_collider(path), "{path}");
        }
    }

    /// A GLB of `json`'s JSON chunk, then a binary chunk the reader never reaches.
    pub(crate) fn glb_of(path: &Path, json: &str) {
        let mut bytes = b"glTF\x02\0\0\0\0\0\0\0".to_vec();
        bytes.extend((json.len() as u32).to_le_bytes());
        bytes.extend(b"JSON");
        bytes.extend(json.as_bytes());
        bytes.extend(b"\0\0\0\0BIN\0");
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_glb_is_empty_with_no_mesh_and_no_animation_in_its_json_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.glb");
        for (json, want) in [
            (r#"{"asset":{"version":"2.0"}}"#, true),
            (r#"{"meshes":[]}  "#, true),
            (r#"{"meshes":[],"animations":[],"nodes":[{}]}"#, true),
            (r#"{"meshes":[{"primitives":[]},{}],"nodes":[{}]}"#, false),
            (r#"{"animations":[{"channels":[]}],"nodes":[{}]}"#, false),
        ] {
            glb_of(&path, json);
            assert_eq!(is_empty_glb(&path), Ok(want), "{json}");
        }
        std::fs::write(&path, "glTF").unwrap();
        assert!(is_empty_glb(&path).is_err());
        glb_of(&path, "{");
        assert!(is_empty_glb(&path).is_err());
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[16..20].copy_from_slice(b"BIN\0");
        std::fs::write(&path, bytes).unwrap();
        assert!(is_empty_glb(&path).is_err());
    }

    #[test]
    fn one_glb_per_model_stem_from_the_fbx_and_none_where_the_folder_ships_one() {
        let files: Vec<Staged> = [
            "Crate.obj",
            "Crate.fbx",
            "Crate.fbx.meta",
            "Props/Barrel.OBJ",
            "Props/barrel.mtl",
            "Props/Lamp.FBX",
            "Props/lamp.fbx",
            "Shipped/Rock.fbx",
            "Shipped/rock.GLB",
            "Shipped/Tree.obj",
            "Other/Rock.fbx",
            "Textures/Crate.png",
        ]
        .map(staged)
        .to_vec();
        let got: Vec<(String, String, Vec<String>)> = models(&files)
            .into_iter()
            .map(|m| (m.path, m.glb, m.source_path))
            .collect();
        let want = [
            // The FBX wins over the OBJ of its stem
            ("Crate.fbx", "Crate.glb"),
            ("Other/Rock.fbx", "Other/Rock.glb"),
            ("Props/Barrel.OBJ", "Props/Barrel.glb"),
            // Stems match ignoring case; the first by path wins
            ("Props/Lamp.FBX", "Props/Lamp.glb"),
            // The folder ships rock.GLB, so Rock.fbx gets none, and Tree.obj still does
            ("Shipped/Tree.obj", "Shipped/Tree.glb"),
        ]
        .map(|(model, glb)| {
            (
                model.to_string(),
                glb.to_string(),
                vec![format!("Assets/Pack/{model}")],
            )
        });
        assert_eq!(got, want);
        assert_eq!(models(&[staged("Readme.txt")]), []);
    }

    #[test]
    fn blender_reads_models_textures_and_unity_files_including_shader_graphs() {
        for path in [
            "Models/SM_Log.fbx",
            "Models/SM_Log.OBJ",
            "Models/SM_Log.mtl",
            "Textures/T_Log_D.PNG",
            "Materials/M_Log.mat",
            "Materials/M_Log.mat.meta",
            "Prefabs/SM_Log.prefab",
            "Scenes/Demo.unity",
            "Shaders/S_Layered.shadergraph",
            "Shaders/S_Layered.ShaderGraph",
        ] {
            assert!(is_input(path), "{path}");
        }
        for path in [
            "Readme.txt",
            "Shaders/S_Layered.shadersubgraph",
            "Shaders/S_Standard.shader",
            "shadergraph",
            ".shadergraph",
        ] {
            assert!(!is_input(path), "{path}");
        }
    }

    fn input(path: &str, source: &str, sha256: &str) -> Input {
        Input {
            path: path.into(),
            source_path: vec![source.into()],
            sha256: sha256.into(),
            guid: None,
        }
    }

    /// A pack's inputs: a model and its `.meta`, a material, its texture, and two textures of one
    /// name in different folders.
    fn pack() -> Vec<Input> {
        vec![
            input("Art/Models/SM_Log.fbx", "Log.fbx", "a"),
            Input {
                guid: Some("a1".repeat(16)),
                ..input("Art/Models/SM_Log.fbx.meta", "Log.fbx.meta", "b")
            },
            input("Art/Materials/M_Log.mat", "M_Log.mat", "c"),
            input("Art/Textures/T_Log_D.png", "T_Log_D.png", "d"),
            input("Art/T_Rock.png", "Rock.png", "e"),
            input("Houses/Art/T_Rock.png", "Houses/Rock.png", "f"),
        ]
    }

    /// `pack` with each input's path changed by `moved`.
    fn moved(moved: impl Fn(&str) -> String) -> Vec<Input> {
        pack()
            .into_iter()
            .map(|i| Input {
                path: moved(&i.path),
                ..i
            })
            .collect()
    }

    #[test]
    fn the_inputs_hash_covers_names_bytes_and_order_but_not_folders() {
        let before = inputs_hash(&pack());
        let mut reversed = pack();
        reversed.reverse();
        assert_eq!(
            inputs_hash(&reversed),
            before,
            "the listing order doesn't count"
        );

        // Hoisting Houses keeps every name and the T_Rock order (art/ < built-in/)
        let hoisted = moved(|p| p.replace("Houses/", "Built-In/"));
        assert_eq!(inputs_hash(&hoisted), before);
        // So does any move that keeps folders' relative order
        let flattened = moved(|p| format!("Pack/Wrap/{p}"));
        assert_eq!(inputs_hash(&flattened), before);

        // Hoisting Art puts T_Rock.png after Houses/Art/T_Rock.png by path
        let reordered = moved(|p| p.strip_prefix("Art/").unwrap_or(p).to_string());
        assert_ne!(inputs_hash(&reordered), before);

        // A rename, an exclude or new bytes change it
        let renamed = moved(|p| p.replace("T_Log_D.png", "T_Log_Base.png"));
        assert_ne!(inputs_hash(&renamed), before);
        let mut excluded = pack();
        excluded.retain(|i| !i.path.ends_with("T_Log_D.png"));
        assert_ne!(inputs_hash(&excluded), before);
        let mut changed = pack();
        changed[3].sha256 = "d2".into();
        assert_ne!(inputs_hash(&changed), before);
    }

    #[test]
    fn the_inputs_hash_follows_every_first_wins_lookup_of_the_script() {
        // Names match as the script's `alnum` reads them: case and punctuation aside
        let pair = |a: &str, b: &str| {
            vec![
                input(&format!("A/{a}"), "one", "1"),
                input(&format!("B/{b}"), "two", "2"),
            ]
        };
        let swap = |inputs: Vec<Input>| -> Vec<Input> {
            inputs
                .into_iter()
                .map(|i| Input {
                    path: if i.path.starts_with("A/") {
                        i.path.replacen("A/", "C/", 1)
                    } else {
                        i.path
                    },
                    ..i
                })
                .collect()
        };
        for (a, b) in [("Bark.png", "bark.tga"), ("T_Bark.png", "t-bark.png")] {
            let inputs = pair(a, b);
            assert_ne!(
                inputs_hash(&swap(inputs.clone())),
                inputs_hash(&inputs),
                "{a} {b}"
            );
        }
        // Names that differ don't
        let inputs = pair("Bark.png", "Moss.png");
        assert_eq!(inputs_hash(&swap(inputs.clone())), inputs_hash(&inputs));
        // Every prefab, every scene, and `.meta` files of one GUID do, whatever their names
        for (a, b) in [("Log.prefab", "Tree.prefab"), ("Demo.unity", "Night.unity")] {
            let inputs = pair(a, b);
            assert_ne!(
                inputs_hash(&swap(inputs.clone())),
                inputs_hash(&inputs),
                "{a} {b}"
            );
        }
        let guid = Some("c3".repeat(16));
        let inputs: Vec<Input> = pair("Log.fbx.meta", "Tree.fbx.meta")
            .into_iter()
            .map(|i| Input {
                guid: guid.clone(),
                ..i
            })
            .collect();
        assert_ne!(inputs_hash(&swap(inputs.clone())), inputs_hash(&inputs));
    }

    #[test]
    fn the_key_covers_the_original_model_options_version_and_blender() {
        let model = models(&[staged("Models/SM_Log.fbx")]).remove(0);
        let options = Options::parse(&Options::default_json()).unwrap();
        let key =
            |original: &str, model: &Model, options: &Options, blender: &str, inputs: &str| {
                glb_key(original, model, options, blender, inputs, None)
            };
        let base = key("orig", &model, &options, "5.2.2", "inputs");
        assert_eq!(base, key("orig", &model, &options, "5.2.2", "inputs"));
        let other_model = Model {
            source_path: vec!["Assets/Pack/Other.fbx".into()],
            ..model.clone()
        };
        let smaller = Options {
            max_texture: 512,
            ..options.clone()
        };
        for other in [
            key("orig2", &model, &options, "5.2.2", "inputs"),
            key("orig", &other_model, &options, "5.2.2", "inputs"),
            key("orig", &model, &smaller, "5.2.2", "inputs"),
            key("orig", &model, &options, "5.2.3", "inputs"),
            key("orig", &model, &options, "5.2.2", "inputs2"),
        ] {
            assert_ne!(other, base);
        }
        // Forcing a conversion doesn't change what it makes
        let forced = Options {
            force: true,
            ..options.clone()
        };
        assert_eq!(key("orig", &model, &forced, "5.2.2", "inputs"), base);
        // The model's path in the pack isn't in it: its name is in the inputs hash
        let placed = Model {
            path: "SM_Log.fbx".into(),
            glb: "SM_Log.glb".into(),
            ..model.clone()
        };
        assert_eq!(key("orig", &placed, &options, "5.2.2", "inputs"), base);

        // A pack with binary Unity files also keys by the converter, each version apart
        let converted =
            |converter: &str| glb_key("orig", &model, &options, "5.2.2", "inputs", Some(converter));
        let now = converter_version();
        assert_ne!(converted(&now), base);
        assert_eq!(converted(&now), converted(&now));
        assert_ne!(
            converted(&now),
            converted("opendam-unity-text/0 UnityPy 1.25.4")
        );
    }

    #[test]
    fn the_converter_version_holds_unity_text_s_version_and_the_locked_unitypy() {
        let engine = include_str!("../../../../../packages/preview-engine/unity_text.py");
        let declared = engine
            .lines()
            .find_map(|l| l.strip_prefix("VERSION = \""))
            .and_then(|rest| rest.strip_suffix('"'));
        assert_eq!(declared, Some(UNITY_TEXT_VERSION));
        let version = converter_version();
        let unitypy = version
            .strip_prefix(&format!("{UNITY_TEXT_VERSION} UnityPy "))
            .unwrap();
        assert!(
            unitypy.split('.').count() == 3 && unitypy.split('.').all(|n| n.parse::<u32>().is_ok()),
            "{version}"
        );
    }

    #[test]
    fn only_a_file_with_the_binary_serialization_s_header_is_binary_unity() {
        // Unity 4.3's header: metadata size, file size, version 9, data offset
        let unity_4 = |len: u32| {
            let mut head = [0xa89u32, len, 9, 0x1000].map(u32::to_be_bytes).concat();
            head.extend(b"4.3.4f1\0");
            head
        };
        assert!(is_serialized_file(&unity_4(4448), 4448));
        assert!(!is_serialized_file(&unity_4(4448), 4449), "another size");
        // Version 22 and later keep the size as 64 bits at byte 24
        let mut unity_2020 = [0u32, 0, 22, 0, 0, 0x2000].map(u32::to_be_bytes).concat();
        unity_2020.extend(9000u64.to_be_bytes());
        assert!(is_serialized_file(&unity_2020, 9000));
        assert!(!is_serialized_file(&unity_2020, 9001));
        for other in [
            &b"%YAML 1.1\n%TAG !u! tag:unity3d.com,2011:\n"[..],
            b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1 a 3ds Max material library",
            b"\xef\xbb\xbf%YAML 1.1\n",
            b"",
        ] {
            assert!(!is_serialized_file(other, other.len() as u64), "{other:?}");
        }
    }

    #[test]
    fn options_default_and_refuse_what_the_step_can_t_run() {
        let default = Options::parse(&serde_json::Value::Null).unwrap();
        assert_eq!(
            default,
            Options {
                max_texture: 1024,
                force: false
            }
        );
        assert_eq!(Options::parse(&Options::default_json()).unwrap(), default);
        for bad in [
            json!({"max_texture": 8}),
            json!({"max_texture": "big"}),
            json!({"maxTexture": 512}),
        ] {
            assert!(Options::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_meta_s_guid_is_read_as_the_script_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let meta = dir.path().join("a.png.meta");
        let guid = "0123456789abcdef0123456789abcdef";
        std::fs::write(&meta, format!("fileFormatVersion: 2\nguid: {guid}\n")).unwrap();
        assert_eq!(meta_guid(&meta).as_deref(), Some(guid));
        std::fs::write(&meta, "fileFormatVersion: 2\nguid: NOTHEX\n").unwrap();
        assert_eq!(meta_guid(&meta), None);
    }
}
