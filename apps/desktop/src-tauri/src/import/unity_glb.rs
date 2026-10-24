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
//! - **The cache.** Each GLB is kept at `<cache>/glb/<key>.glb`, keyed by the original's sha256,
//!   the model's source path, the options, `UNITY_GLB_VERSION`, Blender's version and the
//!   **inputs hash** (`inputs_hash`), so a rebuild that only moved files between folders runs no
//!   Blender at all, and one that dropped, renamed or changed an input converts again.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::build::{Progress, ProgressSink, RunControl, Stop};
use crate::capped;
use crate::export::blender::{self, Blender};
use crate::export::hashes::sha256_bytes;

/// The script's version, which every GLB's cache key holds. The script declares it as
/// `VERSION`, which a test keeps equal: bump both when the GLBs it writes change.
pub const UNITY_GLB_VERSION: &str = "opendam-unity-glb/4";

/// The conversion script, embedded so the installed app needs no file beside it.
const SCRIPT: &str = include_str!("unity_glb.py");

/// Blender's memory cap when Settings sets none (D20).
pub const DEFAULT_MEMORY_MAX: &str = "12G";

const MODEL_EXTENSIONS: [&str; 2] = ["fbx", "obj"];
/// The files the script loads as textures. It reads them from its job (`imageExtensions`), so
/// this is the one list of them.
const IMAGE_EXTENSIONS: [&str; 8] = ["png", "jpg", "jpeg", "tga", "tif", "tiff", "psd", "bmp"];
/// The other files Blender reads besides the models: Unity's `.meta`, `.mat`, `.prefab` and
/// `.unity`, and an OBJ's `.mtl`. A test keeps every extension the script names among these.
const UNITY_EXTENSIONS: [&str; 5] = ["meta", "mat", "prefab", "unity", "mtl"];

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
    pub assimp: String,
    /// The systemd tools Blender's scope runs through.
    pub systemd: capped::Systemd,
}

impl Tools {
    pub async fn load(master: &sqlx::Pool<sqlx::Sqlite>, cache_dir: PathBuf) -> Self {
        let blender_path = match crate::export::settings::load(master).await {
            Ok(settings) => settings.blender_path,
            Err(e) => {
                tracing::warn!("Could not read Blender's path from Settings: {e}");
                None
            }
        };
        let memory_max = match super::settings::load(master).await {
            Ok(settings) => settings.blender_memory_max(),
            Err(e) => {
                tracing::warn!("Could not read the import settings: {e}");
                DEFAULT_MEMORY_MAX.to_string()
            }
        };
        Tools {
            blender_path,
            memory_max,
            cache_dir,
            assimp: "assimp".into(),
            systemd: capped::Systemd::default(),
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

/// A GLB's cache key.
pub fn glb_key(
    original_sha256: &str,
    model: &Model,
    options: &Options,
    blender_version: &str,
    inputs_hash: &str,
) -> String {
    let key = json!({
        "original": original_sha256,
        "model": model.source_path,
        "maxTexture": options.max_texture,
        "version": UNITY_GLB_VERSION,
        "blender": blender_version,
        "inputs": inputs_hash,
    });
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
    /// returns the models that got one. A model Blender fails on fails the step, naming it.
    pub async fn run(&self) -> Result<Vec<Model>, Stop> {
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
        let inputs = blocking(move || {
            Ok(files
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
                .collect::<Vec<_>>())
        })
        .await?;
        let inputs_hash = inputs_hash(&inputs);
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
        blocking(move || {
            for (model, cached) in &placed {
                std::fs::copy(cached, staging.join(&model.glb))
                    .map_err(|e| format!("Could not write {}: {e}", model.glb))?;
            }
            Ok(())
        })
        .await?;
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
        blocking(move || {
            std::fs::create_dir_all(&folders.1)
                .map_err(|e| format!("Could not make the conversion's work folder: {e}"))?;
            for path in copied {
                if control.cancelled() {
                    return Ok(());
                }
                let to = folders.0.join(&path);
                if let Some(parent) = to.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("Could not make the conversion's work folder: {e}"))?;
                }
                std::fs::copy(staging.join(&path), &to)
                    .map_err(|e| format!("Could not copy {path} to the work folder: {e}"))?;
            }
            Ok(())
        })
        .await?;
        self.cancelled()?;

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
            "meta", "prefab", "unity", "mat", "mtl", "fbx", "jpg", "jpeg",
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
                glb_key(original, model, options, blender, inputs)
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
