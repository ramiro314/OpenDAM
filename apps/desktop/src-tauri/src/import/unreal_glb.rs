//! The `export_unreal` step (plan pack-import, Unreal conversion): an Unreal pack's producer. A
//! zip holding a content-only Unreal project becomes a pack of GLBs (materials baked to PNG
//! textures) at the project's `/Game` layout, beside the docs the project brings (D21). The
//! `.uasset` project stays in the zip.
//!
//! - **The project.** Its `.uproject` is found at any depth and any name (`Feif.uproject`). A
//!   project with a `Source/` folder is refused: the prebuilt editor can't compile it.
//! - **A work copy.** The project is unzipped into a work folder of the cache, and the
//!   `PythonScriptPlugin` and `GLTFExporter` plugins are enabled in the copy's `.uproject` only.
//!   The work folder is deleted when the step ends, however it ends.
//! - **Sessions.** The editor runs headless under the Unreal memory cap (`capped`), the embedded
//!   `unreal_glb.py` (a port of `ue_gltf_export.py`) exporting at most `batch` assets per
//!   session. It restarts until nothing is left, and fails the step when a session makes no
//!   progress. A session the cap kills after exporting something is followed by another; one
//!   it kills before fails the step, naming the cap.
//! - **The cache.** Each GLB is kept at `<cache>/glb/<key>.glb` as soon as its session ends,
//!   keyed by the original's sha256, its `/Game` path, `UNREAL_GLB_VERSION` and the editor's
//!   version (`glb_key`), and a record of the run's exports so far (`Record`). The next run
//!   puts the cached GLBs where the script looks first, so an interrupted run resumes, and a
//!   rebuild after a finished one runs no editor and unzips nothing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::build::{Progress, ProgressSink, RunControl, Stop};
use super::unitypackage::{below, ExtractedFile, Extraction, SkipReason, SkippedEntry, Taken};
use super::zipfile;
use crate::capped;
use crate::export::hashes::sha256_bytes;

/// The script's version, which every GLB's cache key holds. The script declares it as
/// `VERSION`, which a test keeps equal: bump both when the GLBs it writes change.
pub const UNREAL_GLB_VERSION: &str = "opendam-unreal-glb/2";

/// The export script, embedded so the installed app needs no file beside it.
const SCRIPT: &str = include_str!("unreal_glb.py");

/// The Unreal Editor's memory cap when Settings sets none (D20).
pub const DEFAULT_MEMORY_MAX: &str = "20G";

/// The editor and its version, below the editor's root.
const EDITOR: &str = "Engine/Binaries/Linux/UnrealEditor";
const BUILD_VERSION: &str = "Engine/Build/Build.version";

/// The plugins the export needs, enabled in the work copy's `.uproject`.
const PLUGINS: [&str; 2] = ["PythonScriptPlugin", "GLTFExporter"];

/// The files of a project that come along as its docs, at its root or below `Content/`.
const DOCS: [&str; 4] = ["txt", "md", "pdf", "rtf"];

const STEP: &str = "exporting from Unreal";
const NO_PROJECT: &str = "The zip holds no Unreal project (.uproject)";

/// The step's options: how many assets one editor session exports at most.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    #[serde(default = "default_batch")]
    pub batch: u32,
}

fn default_batch() -> u32 {
    30
}

impl Options {
    pub fn parse(options: &serde_json::Value) -> Result<Self, String> {
        let options = match options {
            serde_json::Value::Null => json!({}),
            other => other.clone(),
        };
        let parsed: Options = serde_json::from_value(options)
            .map_err(|e| format!("The export_unreal step's options are invalid: {e}"))?;
        if !(1..=1000).contains(&parsed.batch) {
            return Err("The export_unreal step's batch must be 1 to 1000 assets".into());
        }
        Ok(parsed)
    }

    /// The step's default options, as an import records them.
    pub fn default_json() -> serde_json::Value {
        json!({ "batch": default_batch() })
    }
}

/// What the step runs with, read from Settings for each run.
#[derive(Debug, Clone)]
pub struct Tools {
    /// `import.unrealEditorRoot`.
    pub editor_root: PathBuf,
    /// `import.unrealMemoryMax`.
    pub memory_max: String,
    /// The import's cache folder: `glb/` for the GLBs, `unreal/` for the records, `work/` for
    /// the work copies.
    pub cache_dir: PathBuf,
    /// The systemd tools the editor's scope runs through.
    pub systemd: capped::Systemd,
}

/// The editor a run starts, and its version.
#[derive(Debug, Clone)]
pub struct Editor {
    pub program: PathBuf,
    pub version: String,
}

/// `~/Applications/UnrealEngine`, where the editor's Linux zip is extracted.
pub fn default_editor_root() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Applications/UnrealEngine")
}

impl Tools {
    pub async fn load(master: &sqlx::Pool<sqlx::Sqlite>, cache_dir: PathBuf) -> Self {
        let settings = super::settings::load(master).await.unwrap_or_else(|e| {
            tracing::warn!("Could not read the import settings: {e}");
            Default::default()
        });
        Tools {
            editor_root: settings.unreal_editor_root(),
            memory_max: settings.unreal_memory_max(),
            cache_dir,
            systemd: capped::Systemd::default(),
        }
    }

    /// The editor below the root, with its version from `Engine/Build/Build.version`, or why
    /// the step can't run here.
    pub fn editor(&self) -> Result<Editor, String> {
        use std::os::unix::fs::PermissionsExt;
        let needs = |why: String| format!("Exporting from Unreal needs the Unreal Editor: {why}");
        let program = self.editor_root.join(EDITOR);
        let runs = std::fs::metadata(&program)
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
        if !runs {
            return Err(needs(format!(
                "no editor at {} (set its folder in Settings)",
                program.display()
            )));
        }
        let file = self.editor_root.join(BUILD_VERSION);
        let read = std::fs::read(&file)
            .map_err(|e| e.to_string())
            .and_then(|bytes| {
                serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|e| e.to_string())
            });
        let version = read.ok().and_then(|v| {
            let part = |key: &str| v[key].as_u64();
            let mut version = format!(
                "{}.{}.{}",
                part("MajorVersion")?,
                part("MinorVersion")?,
                part("PatchVersion")?
            );
            if let Some(change) = part("Changelist") {
                version.push_str(&format!("+{change}"));
            }
            Some(version)
        });
        let version = version
            .ok_or_else(|| needs(format!("its version can't be read from {}", file.display())))?;
        Ok(Editor { program, version })
    }

    /// Why the step can't run here, `None` when it can.
    pub fn unavailable(&self) -> Option<String> {
        self.editor().err()
    }
}

fn extension(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?;
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty()).then_some(ext)
}

fn has_extension(path: &str, wanted: &[&str]) -> bool {
    extension(path).is_some_and(|ext| wanted.iter().any(|w| ext.eq_ignore_ascii_case(w)))
}

/// `path` below `folder`, the folder's name compared ignoring ASCII case.
fn below_named<'p>(path: &'p str, folder: &str) -> Option<&'p str> {
    let (first, rest) = path.split_once('/')?;
    first.eq_ignore_ascii_case(folder).then_some(rest)
}

/// A pack path below the project's `Content/` (`Fixture/Meshes/SM_A.uasset`) as its `/Game`
/// path, with no extension (`/Game/Fixture/Meshes/SM_A`).
fn game_path(content_rel: &str) -> String {
    let stem = content_rel
        .rsplit_once('.')
        .map_or(content_rel, |(stem, _)| stem);
    format!("/Game/{stem}")
}

/// The GLB of the asset at `/Game` path `game`, relative to the pack root.
fn glb_path(game: &str) -> String {
    format!("{}.glb", game.strip_prefix("/Game/").unwrap_or(game))
}

/// An Unreal project in a zip, as its listing names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The `.uproject`'s folder in the zip (clean), `None` at the zip's root.
    pub dir: Option<String>,
    /// The `.uproject`'s file name.
    pub file: String,
    /// Whether it has a `Source/` folder.
    pub has_source: bool,
    /// Its first file of native code the editor would load, by its path in the project: one
    /// below a `Binaries/` folder anywhere, or below a plugin's `Source/`.
    pub native: Option<String>,
    /// The `/Game` path of every `.uasset` below `Content/`, sorted. Each may become one GLB.
    pub assets: Vec<String>,
    /// The docs it brings, at their paths in the pack, each with its name in the zip: those at
    /// the project's root keep their names, and those below `Content/` their paths below it.
    pub docs: Vec<ExtractedFile>,
}

impl Project {
    /// The project of a zip whose listing is `zip` (`ZipIndex::list(None)`), `None` when it
    /// holds no `.uproject`. Two projects, or two docs landing at one path, are refused.
    pub fn find(zip: &Extraction) -> Result<Option<Project>, String> {
        let uprojects: Vec<&ExtractedFile> = zip
            .files
            .iter()
            .filter(|f| has_extension(&f.path, &["uproject"]))
            .collect();
        let uproject = match uprojects[..] {
            [] => return Ok(None),
            [one] => one,
            _ => {
                let names: Vec<&str> = uprojects.iter().map(|f| f.path.as_str()).collect();
                return Err(format!(
                    "The zip holds more than one Unreal project: {}",
                    names.join(", ")
                ));
            }
        };
        let (dir, file) = match uproject.path.rsplit_once('/') {
            Some((dir, file)) => (Some(dir.to_string()), file.to_string()),
            None => (None, uproject.path.clone()),
        };
        let mut project = Project {
            dir,
            file,
            has_source: false,
            native: None,
            assets: Vec::new(),
            docs: Vec::new(),
        };
        let mut taken = Taken::default();
        for f in &zip.files {
            let rel = match &project.dir {
                Some(dir) => match below(&f.path, dir) {
                    Some(rel) if !rel.is_empty() => rel,
                    _ => continue,
                },
                None => f.path.as_str(),
            };
            if below_named(rel, "Source").is_some() {
                project.has_source = true;
            }
            let folders: Vec<&str> = rel.split('/').collect();
            let folders = &folders[..folders.len() - 1];
            let binaries = folders.iter().any(|f| f.eq_ignore_ascii_case("Binaries"));
            let plugin_source = folders
                .first()
                .is_some_and(|f| f.eq_ignore_ascii_case("Plugins"))
                && folders.iter().any(|f| f.eq_ignore_ascii_case("Source"));
            if (binaries || plugin_source) && project.native.is_none() {
                project.native = Some(rel.to_string());
            }
            let content = below_named(rel, "Content");
            if let Some(asset) = content.filter(|c| has_extension(c, &["uasset"])) {
                project.assets.push(game_path(asset));
            }
            let doc = match content {
                Some(content) => Some(content),
                None => (!rel.contains('/')).then_some(rel),
            };
            if let Some(doc) = doc.filter(|d| has_extension(d, &DOCS)) {
                taken
                    .claim(doc, "project's docs")
                    .map_err(|e| format!("The Unreal project's docs clash: {e}"))?;
                project.docs.push(ExtractedFile {
                    path: doc.to_string(),
                    ..f.clone()
                });
            }
        }
        project.assets.sort();
        project.docs.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Some(project))
    }

    /// The `.uproject`'s path in the zip.
    pub fn path(&self) -> String {
        match &self.dir {
            Some(dir) => format!("{dir}/{}", self.file),
            None => self.file.clone(),
        }
    }

    /// Why the project can't be exported: a `Source/` folder.
    ///
    /// The export opens the project in an editor running as you, with its Python plugin on, so
    /// a project's own code must not run: one that ships compiled code, which the editor would
    /// load, is refused too, and the work copy's Python startup scripts are removed
    /// (`disarm_python`). What remains trusted is the editor's reading of the project's assets.
    pub fn refusal(&self) -> Option<String> {
        if self.has_source {
            return Some(format!(
                "The Unreal project {} has a Source/ folder: the prebuilt editor can't compile \
                 its C++ code, so only a content-only project can be imported",
                self.file
            ));
        }
        self.native.as_ref().map(|path| {
            format!(
                "The Unreal project {} ships compiled code ({path}), which the editor would \
                 load and run: only a content-only project can be imported",
                self.file
            )
        })
    }

    /// The files an export could write at most, for a preview: a GLB for every `.uasset`, at
    /// most, and the docs. Only the editor knows which assets are meshes or animations.
    pub fn estimate(&self) -> Vec<ExtractedFile> {
        let mut files: Vec<ExtractedFile> = self
            .assets
            .iter()
            .map(|game| ExtractedFile {
                path: glb_path(game),
                source_path: vec![game.clone()],
                size: 0,
            })
            .chain(self.docs.iter().cloned())
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files
    }
}

/// A GLB's cache key. The step's `batch` isn't in it: it changes only how many sessions make
/// the GLBs, not what they hold.
pub fn glb_key(original_sha256: &str, game: &str, editor_version: &str) -> String {
    let key = json!({
        "original": original_sha256,
        "asset": game,
        "version": UNREAL_GLB_VERSION,
        "editor": editor_version,
    });
    sha256_bytes(key.to_string().as_bytes())
}

/// The key of a run's record, over everything but the asset.
fn record_key(original_sha256: &str, editor_version: &str) -> String {
    let key = json!({
        "original": original_sha256,
        "version": UNREAL_GLB_VERSION,
        "editor": editor_version,
    });
    sha256_bytes(key.to_string().as_bytes())
}

/// What the runs over one original have exported, by `/Game` path, rewritten after every
/// session: the GLBs made, the assets the exporter failed on, and whether a session reported
/// nothing left.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub exported: BTreeSet<String>,
    pub failed: BTreeSet<String>,
    pub complete: bool,
}

/// One run of the step: the export of the zip `original` into the empty `staging` folder.
pub struct Export<'a> {
    pub original: &'a Path,
    pub original_sha256: &'a str,
    pub staging: &'a Path,
    pub options: &'a Options,
    pub tools: &'a Tools,
    pub control: Arc<RunControl>,
    pub progress: ProgressSink,
}

/// Deletes a work folder when the step ends, however it ends.
struct WorkFolder(PathBuf);

impl WorkFolder {
    /// Deletes the folder off the async runtime, as an unzipped project can be gigabytes. The
    /// `Drop` below is the fallback for a step that panicked.
    async fn remove(mut self) {
        let path = std::mem::take(&mut self.0);
        let _ = tokio::task::spawn_blocking(move || remove_work_folder(&path)).await;
    }
}

impl Drop for WorkFolder {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            remove_work_folder(&self.0);
        }
    }
}

fn remove_work_folder(path: &Path) {
    if let Err(e) = std::fs::remove_dir_all(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("Could not delete the work folder {}: {e}", path.display());
        }
    }
}

impl Export<'_> {
    fn cancelled(&self) -> Result<(), Stop> {
        if self.control.cancelled() {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    fn cached(&self, editor: &Editor, game: &str) -> PathBuf {
        let key = glb_key(self.original_sha256, game, &editor.version);
        self.tools.cache_dir.join("glb").join(format!("{key}.glb"))
    }

    /// Writes the pack's GLBs and docs into the staging folder, as the extracted files, and
    /// returns them with the entries skipped: the project's unsafe entries, and the assets the
    /// exporter failed on.
    pub async fn run(&self) -> Result<Extraction, Stop> {
        (self.progress)(Progress {
            step: STEP,
            done: 0,
            total: 0,
        });
        let editor = self.tools.editor()?;
        let original = self.original.to_path_buf();
        let (project, listing, doc_entries) = blocking(move || {
            let read = |e: anyhow::Error| format!("The original can't be read: {e:#}");
            let index = zipfile::index_zip(&original).map_err(read)?;
            let project = Project::find(&index.list(None).map_err(read)?)?
                .ok_or_else(|| NO_PROJECT.to_string())?;
            let listing = index.list(project.dir.as_deref()).map_err(read)?;
            // Each doc's entry, by the index the listing read it at: its name may not be the
            // zip's own (an undecodable one is read lossily), so it isn't looked up by name.
            let entries = index.file_entries();
            let doc_entries = project
                .docs
                .iter()
                .map(|doc| {
                    let name = &doc.source_path[0];
                    let entry = entries.get(name).copied();
                    entry.ok_or_else(|| format!("{name} isn't among the zip's files"))
                })
                .collect::<Result<Vec<usize>, String>>()?;
            Ok((project, listing, doc_entries))
        })
        .await?;
        if let Some(why) = project.refusal() {
            return Err(Stop::Failed(why));
        }

        let record_file = self.tools.cache_dir.join("unreal").join(format!(
            "{}.json",
            record_key(self.original_sha256, &editor.version)
        ));
        let mut record = read_record(&record_file).await;
        let done = record.complete
            && record
                .exported
                .iter()
                .all(|game| self.cached(&editor, game).is_file());
        if !done {
            record = self
                .export(&editor, &project, &listing, record, &record_file)
                .await?;
        }
        self.cancelled()?;

        let glbs: Vec<(String, PathBuf)> = record
            .exported
            .iter()
            .map(|game| (game.clone(), self.cached(&editor, game)))
            .collect();
        let (original, staging, docs) = (
            self.original.to_path_buf(),
            self.staging.to_path_buf(),
            project.docs.clone(),
        );
        let mut files = blocking(move || {
            let mut files = Vec::new();
            for (game, cached) in glbs {
                let path = glb_path(&game);
                let to = staging.join(&path);
                let written = std::fs::create_dir_all(to.parent().expect("in the pack"))
                    .and_then(|()| std::fs::copy(&cached, &to));
                let size = written.map_err(|e| format!("Could not write {path}: {e}"))?;
                files.push(ExtractedFile {
                    path,
                    source_path: vec![game],
                    size,
                });
            }
            let mut zip = zipfile::open(&original)
                .map_err(|e| format!("The original can't be read: {e:#}"))?;
            for (doc, entry) in docs.into_iter().zip(doc_entries) {
                let size = zipfile::copy_entry(&mut zip, entry, &staging.join(&doc.path))
                    .map_err(|e| format!("Could not write {}: {e:#}", doc.path))?;
                files.push(ExtractedFile { size, ..doc });
            }
            Ok(files)
        })
        .await?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let mut skipped = listing.skipped;
        skipped.extend(record.failed.iter().map(|game| SkippedEntry {
            source_path: vec![game.clone()],
            reason: SkipReason::NotExported,
        }));
        Ok(Extraction { files, skipped })
    }

    /// Exports the project in editor sessions over a work copy, until a session reports nothing
    /// left, starting from the GLBs `record` and the cache already hold. Each session's new
    /// GLBs go into the cache, and the record is rewritten, as soon as it ends. The work folder
    /// is deleted however the export ends.
    async fn export(
        &self,
        editor: &Editor,
        project: &Project,
        listing: &Extraction,
        record: Record,
        record_file: &Path,
    ) -> Result<Record, Stop> {
        let work = WorkFolder(
            self.tools
                .cache_dir
                .join("work")
                .join(uuid::Uuid::new_v4().to_string()),
        );
        let exported = self
            .export_in(&work.0, editor, project, listing, record, record_file)
            .await;
        work.remove().await;
        exported
    }

    /// `export`, in the work folder `work`.
    async fn export_in(
        &self,
        work: &Path,
        editor: &Editor,
        project: &Project,
        listing: &Extraction,
        mut record: Record,
        record_file: &Path,
    ) -> Result<Record, Stop> {
        let copy = work.join("project");
        let out = work.join("out");
        let needed: u64 = listing.files.iter().map(|f| f.size).sum();
        (self.progress)(Progress {
            step: "unzipping the Unreal project",
            done: 0,
            total: needed,
        });
        let (original, dir, to) = (
            self.original.to_path_buf(),
            project.dir.clone(),
            copy.clone(),
        );
        let uproject = copy.join(&project.file);
        let edited = uproject.clone();
        blocking(move || {
            // Needs the project's size free in the cache's folder.
            zipfile::extract_zip(&original, &to, dir.as_deref()).map_err(|e| {
                format!("Unzipping the Unreal project into its work folder failed: {e:#}")
            })?;
            disarm_python(edited.parent().expect("in the work copy"))?;
            enable_plugins(&edited)
        })
        .await?;
        self.cancelled()?;

        // The GLBs cached so far go where the script looks first, so it exports only the rest.
        let seeds: Vec<(String, PathBuf)> = record
            .exported
            .iter()
            .chain(&project.assets)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|game| (game.clone(), self.cached(editor, game)))
            .filter(|(_, cached)| cached.is_file())
            .collect();
        let seeded = out.clone();
        blocking(move || {
            std::fs::create_dir_all(&seeded).map_err(|e| e.to_string())?;
            for (game, cached) in seeds {
                let to = seeded.join(glb_path(&game));
                std::fs::create_dir_all(to.parent().expect("in the folder"))
                    .and_then(|()| std::fs::copy(&cached, &to))
                    .map_err(|e| format!("Could not copy the cached GLB of {game}: {e}"))?;
            }
            Ok(())
        })
        .await?;
        let script = work.join("unreal_glb.py");
        tokio::fs::write(&script, SCRIPT)
            .await
            .map_err(|e| format!("Could not write the export script: {e}"))?;

        // The work folder is new, so no asset has failed in it yet.
        record.failed.clear();
        loop {
            self.cancelled()?;
            let before = exported_in(&out).await?;
            let _ = tokio::fs::remove_file(out.join(".gltf-remaining")).await;
            // However the session ends (killed by the cap, cancelled, done), the GLBs it
            // finished are whole: the script renames each into place once written. They go
            // into the cache before the step stops, so the next run resumes after them.
            let ended = self.session(editor, &uproject, &script, &out, work).await;
            let after = exported_in(&out).await?;
            for game in after.difference(&before) {
                super::unity_glb::publish(
                    &out.join(glb_path(game)),
                    &self.cached(editor, game),
                    "The Unreal Editor",
                )
                .await
                .map_err(|e| format!("Exporting {game} from Unreal failed: {e}"))?;
            }
            let failed = failed_in(&out).await;
            // An asset the exporter failed on is done too: later sessions skip it.
            let progressed = after.len() > before.len() || failed.len() > record.failed.len();
            record.exported = after;
            record.failed = failed;
            let remaining = tokio::fs::read_to_string(out.join(".gltf-remaining"))
                .await
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok());
            record.complete = remaining == Some(0);
            write_record(record_file, &record).await?;
            let (output, killed) = ended?;
            if record.complete {
                return Ok(record);
            }
            // A session the cap killed after exporting something leaves the rest to a fresh
            // editor, whose memory starts over.
            if let (Some(killed), false) = (&killed, progressed) {
                return Err(Stop::Failed(format!(
                    "Exporting from Unreal failed: the Unreal Editor was {killed}"
                )));
            }
            if let Some(killed) = killed {
                tracing::warn!(
                    "The Unreal Editor was {killed} mid-batch; starting another session"
                );
                continue;
            }
            if !progressed {
                let left = match remaining {
                    Some(n) => format!("with {n} assets left"),
                    None => "without saying what is left".to_string(),
                };
                return Err(Stop::Failed(format!(
                    "Exporting from Unreal failed: an editor session made no progress, {left} \
                     ({}){}",
                    output.status,
                    tail(&output)
                )));
            }
        }
    }

    /// One editor session over the work copy, under the memory cap, counting the assets it
    /// reports for the progress, and stopped on Cancel. Returns its output, with why the cap
    /// killed it when it did.
    async fn session(
        &self,
        editor: &Editor,
        uproject: &Path,
        script: &Path,
        out: &Path,
        dir: &Path,
    ) -> Result<(std::process::Output, Option<String>), Stop> {
        let run_script = format!("-ExecutePythonScript={}", script.display());
        let args = [
            uproject.as_os_str(),
            run_script.as_ref(),
            "-unattended".as_ref(),
            "-nosplash".as_ref(),
            "-stdout".as_ref(),
            "-FullStdOutLogOutput".as_ref(),
        ];
        let mut capped = capped::capped_command_with(
            &self.tools.systemd,
            &editor.program,
            args,
            &self.tools.memory_max,
        )
        .await;
        capped
            .command
            .current_dir(dir)
            .env("GLTF_OUT", out)
            .env("GLTF_BATCH", self.options.batch.to_string());
        let (mut done, mut total) = (0u64, 0u64);
        // Uncapped, `capped_command_with` has warned.
        let output = capped
            .watch(
                |line| {
                    let line = String::from_utf8_lossy(line);
                    let Some((_, said)) = line.split_once("[gltf] ") else {
                        return;
                    };
                    // "N assets, M left, ..." opens a session; each asset then reports its end.
                    let counts = said.split_once(" assets, ").and_then(|(all, rest)| {
                        let left = rest.split_once(" left")?.0;
                        Some((all.parse::<u64>().ok()?, left.parse::<u64>().ok()?))
                    });
                    if let Some((all, left)) = counts {
                        (total, done) = (all, all.saturating_sub(left));
                    } else if said.starts_with("exported /") || said.starts_with("no file written")
                    {
                        done += 1;
                    } else {
                        return;
                    }
                    (self.progress)(Progress {
                        step: STEP,
                        done: done.min(total),
                        total,
                    });
                },
                || self.control.cancelled(),
            )
            .await
            .map_err(|e| format!("Exporting from Unreal failed: could not run the editor: {e}"))?
            .ok_or(Stop::Cancelled)?;
        let killed = capped.killed_by_cap(output.status);
        Ok((output, killed))
    }
}

/// The last lines a session printed, as `: a / b / c`, or nothing.
fn tail(output: &std::process::Output) -> String {
    let printed = [&output.stdout, &output.stderr]
        .into_iter()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .find(|text| !text.trim().is_empty())
        .unwrap_or_default();
    let lines: Vec<&str> = printed.lines().filter(|l| !l.trim().is_empty()).collect();
    match lines[lines.len().saturating_sub(3)..].join(" / ") {
        tail if tail.is_empty() => tail,
        tail => format!(": {tail}"),
    }
}

/// The `/Game` paths of the GLBs below the script's output folder `out`. Hidden files, such as
/// the script's partial GLBs, don't count.
async fn exported_in(out: &Path) -> Result<BTreeSet<String>, Stop> {
    let out = out.to_path_buf();
    blocking(move || {
        let mut found = BTreeSet::new();
        let walk = walkdir::WalkDir::new(&out)
            .min_depth(1)
            .into_iter()
            .filter_entry(|e| !e.file_name().to_string_lossy().starts_with('.'));
        for entry in walk {
            let entry = entry.map_err(|e| format!("Reading the editor's output failed: {e}"))?;
            let rel = entry.path().strip_prefix(&out).expect("below the folder");
            let rel = rel.to_string_lossy();
            if entry.file_type().is_file() {
                if let Some(stem) = rel.strip_suffix(".glb") {
                    found.insert(format!("/Game/{stem}"));
                }
            }
        }
        Ok(found)
    })
    .await
}

/// The `/Game` paths the script failed on in this work folder, from its `.gltf-failed`, one per
/// line: a path can hold spaces (`/Game/My Pack/SM_Rock`).
async fn failed_in(out: &Path) -> BTreeSet<String> {
    let failed = tokio::fs::read_to_string(out.join(".gltf-failed"))
        .await
        .unwrap_or_default();
    failed
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// The `.ini` sections through which a project names code for the editor to run as it starts,
/// with the Python plugin on: the plugin's settings (`StartupScripts`, and `AdditionalPaths`,
/// whose folders' `init_unreal.py` would run and could shadow the modules the export imports),
/// and the editor utility subsystem's `StartupObjects`, a Blueprint that can run Python.
const STARTUP_SECTIONS: [&str; 3] = [
    "/Script/PythonScriptPlugin.PythonScriptPluginSettings",
    "/Script/PythonScriptPlugin.PythonScriptPluginUserSettings",
    "/Script/Blutility.EditorUtilitySubsystem",
];

/// Removes from the work copy `copy` the startup hooks the editor would run of the project's
/// own once the export turns its Python plugin on: every `Python/` folder directly below a
/// `Content/` folder (its `init_unreal.py`, the project's or a plugin's), and in every `.ini`
/// file, the `STARTUP_SECTIONS` and any `StartupScripts` entry.
fn disarm_python(copy: &Path) -> Result<(), String> {
    let failed = |e: String| format!("Could not prepare the work copy: {e}");
    let mut python = Vec::new();
    let mut inis = Vec::new();
    for entry in walkdir::WalkDir::new(copy).min_depth(1) {
        let entry = entry.map_err(|e| failed(e.to_string()))?;
        let name = entry.file_name().to_string_lossy();
        let parent = entry.path().parent().and_then(|p| p.file_name());
        let in_content = parent.is_some_and(|p| p.eq_ignore_ascii_case("Content"));
        if entry.file_type().is_dir() && in_content && name.eq_ignore_ascii_case("Python") {
            python.push(entry.into_path());
        } else if entry.file_type().is_file() && has_extension(&name, &["ini"]) {
            inis.push(entry.into_path());
        }
    }
    for folder in python {
        std::fs::remove_dir_all(&folder).map_err(|e| failed(e.to_string()))?;
    }
    for ini in inis.into_iter().filter(|ini| ini.exists()) {
        let text = std::fs::read_to_string(&ini).map_err(|e| failed(e.to_string()))?;
        let kept = without_startup_hooks(&text);
        if kept != text {
            std::fs::write(&ini, kept).map_err(|e| failed(e.to_string()))?;
        }
    }
    Ok(())
}

/// The `.ini` text `text` without its `STARTUP_SECTIONS` (header and entries) and without any
/// `StartupScripts` entry elsewhere, every other line kept as it is.
fn without_startup_hooks(text: &str) -> String {
    let mut dropping = false;
    let mut kept = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if let Some(section) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            dropping = STARTUP_SECTIONS
                .iter()
                .any(|hook| section.trim().eq_ignore_ascii_case(hook));
        }
        let key = trimmed.trim_start_matches(['+', '-', '!', '.']);
        let startup = key
            .split('=')
            .next()
            .is_some_and(|k| k.trim().eq_ignore_ascii_case("StartupScripts"));
        if !dropping && !startup {
            kept.push_str(line);
        }
    }
    kept
}

/// Enables `PLUGINS` in the `.uproject` at `path`, a work copy: an entry it lists is turned on,
/// and one it doesn't is added. A UTF-8 byte order mark is read and dropped.
fn enable_plugins(path: &Path) -> Result<(), String> {
    let invalid = |why: String| format!("The work copy's {} can't be read: {why}", path.display());
    let text = std::fs::read_to_string(path).map_err(|e| invalid(e.to_string()))?;
    let mut project: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| invalid(e.to_string()))?;
    let plugins = project
        .as_object_mut()
        .ok_or_else(|| invalid("it isn't a JSON object".into()))?
        .entry("Plugins")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| invalid("its Plugins isn't a list".into()))?;
    for name in PLUGINS {
        let listed = plugins
            .iter_mut()
            .find(|p| p.is_object() && p["Name"] == name);
        match listed {
            Some(plugin) => plugin["Enabled"] = true.into(),
            None => plugins.push(json!({ "Name": name, "Enabled": true })),
        }
    }
    let mut written = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    project
        .serialize(&mut serde_json::Serializer::with_formatter(
            &mut written,
            formatter,
        ))
        .map_err(|e| invalid(e.to_string()))?;
    std::fs::write(path, written).map_err(|e| invalid(e.to_string()))
}

/// The record at `file`, empty when there is none or it can't be read.
async fn read_record(file: &Path) -> Record {
    let text = tokio::fs::read(file).await.unwrap_or_default();
    serde_json::from_slice(&text).unwrap_or_default()
}

/// Writes the record at `file`, beside it first, then renamed, so it is always whole.
async fn write_record(file: &Path, record: &Record) -> Result<(), Stop> {
    let dir = file.parent().expect("in the cache folder");
    let partial = dir.join(format!(".{}.partial", uuid::Uuid::new_v4()));
    let text = serde_json::to_vec(record).map_err(|e| e.to_string())?;
    let written = async {
        tokio::fs::create_dir_all(dir).await?;
        tokio::fs::write(&partial, text).await?;
        tokio::fs::rename(&partial, file).await
    }
    .await;
    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(Stop::Failed(format!(
            "Could not write {}: {e}",
            file.display()
        )));
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, Stop> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("The export's worker stopped: {e}"))?
        .map_err(Stop::Failed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::import::unitypackage::tests::fixture;
    use crate::import::zipfile::list_zip;

    #[test]
    fn the_script_declares_the_version_rust_records() {
        let declared = SCRIPT
            .lines()
            .find_map(|l| l.strip_prefix("VERSION = \""))
            .and_then(|rest| rest.strip_suffix('"'));
        assert_eq!(declared, Some(UNREAL_GLB_VERSION));
    }

    #[test]
    fn a_zip_s_project_is_found_at_any_depth_with_its_assets_and_docs() {
        let project = Project::find(&list_zip(&fixture("unreal.zip"), None).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(project.dir.as_deref(), Some("Feif"));
        assert_eq!(project.file, "Feif.uproject");
        assert_eq!(project.path(), "Feif/Feif.uproject");
        assert!(!project.has_source);
        assert_eq!(project.refusal(), None);
        assert_eq!(
            project.assets,
            [
                "/Game/Fixture/Meshes/SM_A",
                "/Game/Fixture/Meshes/SM_B",
                "/Game/Fixture/Meshes/SM_C",
                "/Game/Fixture/Textures/T_A",
            ]
        );
        // Docs at the project root and below Content/, never from Config/; a level is no asset.
        let docs: Vec<(&str, &str)> = project
            .docs
            .iter()
            .map(|d| (d.path.as_str(), d.source_path[0].as_str()))
            .collect();
        assert_eq!(
            docs,
            [
                ("Fixture/Readme.txt", "Feif/Content/Fixture/Readme.txt"),
                ("README.md", "Feif/README.md"),
            ]
        );
        let estimate: Vec<(String, Vec<String>)> = project
            .estimate()
            .into_iter()
            .map(|f| (f.path, f.source_path))
            .collect();
        assert_eq!(
            estimate[0],
            (
                "Fixture/Meshes/SM_A.glb".into(),
                vec!["/Game/Fixture/Meshes/SM_A".into()]
            )
        );
        assert_eq!(estimate.len(), 6);
    }

    #[test]
    fn a_project_with_source_or_a_second_project_is_refused() {
        let source = Project::find(&list_zip(&fixture("unreal-source.zip"), None).unwrap())
            .unwrap()
            .unwrap();
        assert!(source.has_source);
        assert_eq!(
            source.refusal().as_deref(),
            Some(
                "The Unreal project Feif.uproject has a Source/ folder: the prebuilt editor \
                 can't compile its C++ code, so only a content-only project can be imported"
            )
        );

        let listing = |paths: &[&str]| Extraction {
            files: paths
                .iter()
                .map(|p| ExtractedFile {
                    path: p.to_string(),
                    source_path: vec![p.to_string()],
                    size: 1,
                })
                .collect(),
            skipped: Vec::new(),
        };
        assert_eq!(Project::find(&listing(&["Pack/Readme.txt"])), Ok(None));
        assert_eq!(
            Project::find(&listing(&["A/A.uproject", "B/B.uproject"])),
            Err("The zip holds more than one Unreal project: A/A.uproject, B/B.uproject".into())
        );
        let at_root = Project::find(&listing(&["P.uproject", "Content/SM.uasset", "Notes.txt"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            (at_root.dir.clone(), at_root.path()),
            (None, "P.uproject".into())
        );
        assert_eq!(at_root.assets, ["/Game/SM"]);
        // A doc below Content/ and one at the root can't land at one path.
        let clash = Project::find(&listing(&[
            "P.uproject",
            "Readme.txt",
            "Content/Readme.txt",
        ]));
        assert!(
            clash.as_ref().is_err_and(|e| e.contains("Readme.txt")),
            "{clash:?}"
        );
    }

    #[tokio::test]
    async fn failed_assets_are_read_one_per_line_spaces_and_all() {
        let dir = tempfile::tempdir().unwrap();
        assert!(failed_in(dir.path()).await.is_empty());
        std::fs::write(
            dir.path().join(".gltf-failed"),
            "/Game/My Pack/SM_Rock\n\n/Game/Other/SM_Tree\r\n",
        )
        .unwrap();
        assert_eq!(
            failed_in(dir.path()).await.into_iter().collect::<Vec<_>>(),
            ["/Game/My Pack/SM_Rock", "/Game/Other/SM_Tree"]
        );
    }

    #[test]
    fn a_project_shipping_compiled_code_is_refused() {
        let listing = |paths: &[&str]| Extraction {
            files: paths
                .iter()
                .map(|p| ExtractedFile {
                    path: p.to_string(),
                    source_path: vec![p.to_string()],
                    size: 1,
                })
                .collect(),
            skipped: Vec::new(),
        };
        let refusal = |paths: &[&str]| {
            let mut all = vec!["P/P.uproject", "P/Content/SM.uasset"];
            all.extend_from_slice(paths);
            Project::find(&listing(&all)).unwrap().unwrap().refusal()
        };
        for native in [
            "Binaries/Linux/libUnrealEditor-P.so",
            "Plugins/Water/Binaries/Linux/libWater.so",
            "Plugins/Tools/Water/Source/Water/Water.Build.cs",
        ] {
            let why = refusal(&[&format!("P/{native}")]).unwrap();
            assert!(
                why.contains(&format!("ships compiled code ({native})")),
                "{why}"
            );
        }
        // A content-only plugin, and a level or doc, are fine.
        assert_eq!(
            refusal(&[
                "P/Plugins/Water/Content/M_Water.uasset",
                "P/Content/Maps/Demo.umap"
            ]),
            None
        );
    }

    #[test]
    fn the_work_copy_loses_the_project_s_python_startup() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path();
        let write = |rel: &str, text: &str| {
            let path = copy.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("Content/Python/init_unreal.py", "import os\n");
        write("Plugins/Water/Content/Python/init_unreal.py", "import os\n");
        write("Content/Meshes/SM_A.uasset", "mesh");
        write(
            "Content/Meshes/Python/SM_Snake.uasset",
            "a folder only named Python",
        );
        write(
            "Config/DefaultEngine.ini",
            "[/Script/Engine.RendererSettings]\nr.Nanite=False\n\n\
             [/Script/PythonScriptPlugin.PythonScriptPluginSettings]\n\
             +StartupScripts=/tmp/evil.py\n+AdditionalPaths=(Path=\"/tmp/anywhere\")\n\
             bDeveloperMode=False\n\n\
             [ /script/pythonscriptplugin.pythonscriptpluginsettings ]\n\
             +AdditionalPaths=(Path=\"x\")\n\
             [CoreRedirects]\n+ClassRedirects=(OldName=\"A\",NewName=\"B\")\n\
             StartupScripts=stray.py\n",
        );
        write(
            "Config/DefaultEditorPerProjectUserSettings.ini",
            "[/Script/Blutility.EditorUtilitySubsystem]\n+StartupObjects=/Game/EUW_Run.EUW_Run\n",
        );
        write(
            "Plugins/Water/Config/DefaultWater.ini",
            "[/Script/PythonScriptPlugin.PythonScriptPluginUserSettings]\nbDeveloperMode=True\n",
        );
        write(
            "Config/DefaultGame.ini",
            "[/Script/EngineSettings.GeneralProjectSettings]\n",
        );
        disarm_python(copy).unwrap();
        assert!(!copy.join("Content/Python").exists());
        assert!(!copy.join("Plugins/Water/Content/Python").exists());
        assert!(copy.join("Content/Meshes/SM_A.uasset").exists());
        assert!(copy.join("Content/Meshes/Python/SM_Snake.uasset").exists());
        let read = |rel: &str| std::fs::read_to_string(copy.join(rel)).unwrap();
        // The startup sections go whole, however spelled; every other section stays.
        assert_eq!(
            read("Config/DefaultEngine.ini"),
            "[/Script/Engine.RendererSettings]\nr.Nanite=False\n\n\
             [CoreRedirects]\n+ClassRedirects=(OldName=\"A\",NewName=\"B\")\n"
        );
        assert_eq!(read("Config/DefaultEditorPerProjectUserSettings.ini"), "");
        assert_eq!(read("Plugins/Water/Config/DefaultWater.ini"), "");
        assert_eq!(
            std::fs::read_to_string(copy.join("Config/DefaultGame.ini")).unwrap(),
            "[/Script/EngineSettings.GeneralProjectSettings]\n"
        );
    }

    #[test]
    fn the_key_covers_the_original_asset_version_and_editor() {
        let base = glb_key("orig", "/Game/A/SM_A", "5.4.4+1");
        assert_eq!(base, glb_key("orig", "/Game/A/SM_A", "5.4.4+1"));
        for other in [
            glb_key("orig2", "/Game/A/SM_A", "5.4.4+1"),
            glb_key("orig", "/Game/A/SM_B", "5.4.4+1"),
            glb_key("orig", "/Game/A/SM_A", "5.5.0+1"),
        ] {
            assert_ne!(other, base);
        }
    }

    #[test]
    fn options_default_and_refuse_what_the_step_can_t_run() {
        let default = Options::parse(&serde_json::Value::Null).unwrap();
        assert_eq!(default, Options { batch: 30 });
        assert_eq!(Options::parse(&Options::default_json()).unwrap(), default);
        for bad in [
            json!({"batch": 0}),
            json!({"batch": "many"}),
            json!({"size": 5}),
        ] {
            assert!(Options::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_plugins_are_enabled_whatever_the_uproject_lists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("P.uproject");
        let read = || -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
        };
        std::fs::write(
            &path,
            "\u{feff}{\"FileVersion\": 3, \"Plugins\": [{\"Name\": \"GLTFExporter\", \"Enabled\": false}, {\"Name\": \"Other\", \"Enabled\": true}]}",
        )
        .unwrap();
        enable_plugins(&path).unwrap();
        assert_eq!(
            read()["Plugins"],
            json!([
                {"Name": "GLTFExporter", "Enabled": true},
                {"Name": "Other", "Enabled": true},
                {"Name": "PythonScriptPlugin", "Enabled": true},
            ])
        );
        assert_eq!(read()["FileVersion"], 3);
        std::fs::write(&path, "{\"FileVersion\": 3}").unwrap();
        enable_plugins(&path).unwrap();
        assert_eq!(
            read()["Plugins"],
            json!([
                {"Name": "PythonScriptPlugin", "Enabled": true},
                {"Name": "GLTFExporter", "Enabled": true},
            ])
        );
        std::fs::write(&path, "[]").unwrap();
        assert!(enable_plugins(&path).is_err());
    }

    #[test]
    fn the_editor_is_gated_on_its_binary_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools {
            editor_root: dir.path().to_path_buf(),
            memory_max: DEFAULT_MEMORY_MAX.into(),
            cache_dir: dir.path().join("cache"),
            systemd: capped::Systemd::default(),
        };
        let why = tools.unavailable().unwrap();
        assert!(
            why.starts_with("Exporting from Unreal needs the Unreal Editor: no editor at "),
            "{why}"
        );
        write_editor(dir.path(), "exit 0");
        assert!(tools
            .unavailable()
            .unwrap()
            .contains("its version can't be read"));
        write_version(dir.path());
        assert_eq!(tools.editor().unwrap().version, "5.4.4+1");
        std::fs::write(
            dir.path().join(BUILD_VERSION),
            r#"{"MajorVersion": 5, "MinorVersion": 4, "PatchVersion": 4}"#,
        )
        .unwrap();
        assert_eq!(tools.editor().unwrap().version, "5.4.4");
    }

    /// Writes an editor below `root` that runs `body` as a shell script.
    pub(crate) fn write_editor(root: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let program = root.join(EDITOR);
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Writes the version file of an editor 5.4.4 below `root`.
    pub(crate) fn write_version(root: &Path) {
        let file = root.join(BUILD_VERSION);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            r#"{"MajorVersion": 5, "MinorVersion": 4, "PatchVersion": 4, "Changelist": 1}"#,
        )
        .unwrap();
    }
}
