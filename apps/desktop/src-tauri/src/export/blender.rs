//! Which Blender a game export would run, and whether it is accepted (plan C2, D29), and the
//! jobs it runs: the embedded normalization script, one headless process per model (C9, D30).
//! Only an export with an uncached model or animation job calls `gate`: a texture-only or fully
//! cached export never looks for Blender.

use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::export::collision::Patterns;
use crate::export::fbx_facts::FbxFacts;
use crate::export::packs::Origin;

/// The only major.minor exports accept (D29). CI installs a release of it
/// (`.github/blender.env`), and a test keeps the two in step: bump them together.
pub const BLENDER_SERIES: &str = "5.2";

/// How long `blender --version` may take before it counts as not answering.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);

/// A Blender that answered with its version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blender {
    pub program: PathBuf,
    /// "5.2.2"
    pub version: String,
}

/// What Settings shows about Blender: the program, its version, and why exports can't use it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlenderStatus {
    /// `BLENDER_SERIES`, so Settings names the accepted series without restating it
    pub series: &'static str,
    /// The setting's path, or `blender` found on `PATH`
    pub path: Option<String>,
    pub version: Option<String>,
    /// Whether an export would run it
    pub accepted: bool,
    pub problem: Option<String>,
}

/// The Blender an export job would run, from `export.blenderPath` (`None` means `blender` on
/// `PATH`), refused when it is missing, doesn't answer, or isn't `BLENDER_SERIES`.x.
pub async fn gate(configured: Option<&str>) -> Result<Blender, String> {
    let blender = detect(configured).await?;
    check_series(&blender.version)?;
    Ok(blender)
}

/// `gate`'s answer, for Settings.
pub async fn status(configured: Option<&str>) -> BlenderStatus {
    let path = program(configured, std::env::var_os("PATH").as_deref())
        .filter(|_| configured.is_none_or(|p| check_path(p).is_ok()));
    let mut status = BlenderStatus {
        series: BLENDER_SERIES,
        path: path.as_ref().map(|p| p.display().to_string()),
        ..Default::default()
    };
    match detect(configured).await {
        Ok(blender) => {
            status.problem = check_series(&blender.version).err();
            status.accepted = status.problem.is_none();
            status.version = Some(blender.version);
        }
        Err(problem) => status.problem = Some(problem),
    }
    status
}

/// Finds Blender and asks its version.
pub async fn detect(configured: Option<&str>) -> Result<Blender, String> {
    if let Some(path) = configured {
        check_path(path)?;
    }
    let Some(program) = program(configured, std::env::var_os("PATH").as_deref()) else {
        return Err(match configured {
            Some(path) => format!("No Blender at {path}"),
            None => {
                format!("blender isn't on PATH: install Blender {BLENDER_SERIES} or set its path")
            }
        });
    };
    let output = tokio::time::timeout(
        VERSION_TIMEOUT,
        tokio::process::Command::new(&program)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        format!(
            "{} --version didn't answer in {} s",
            program.display(),
            VERSION_TIMEOUT.as_secs()
        )
    })?
    .map_err(|e| format!("Could not start {}: {e}", program.display()))?;
    let version = parse_version(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| format!("{} didn't report a Blender version", program.display()))?;
    Ok(Blender { program, version })
}

/// Refuses a configured path that isn't absolute, both when it is saved and when it is checked,
/// so Settings never accepts a path relative to the app's working directory that it can't save.
pub fn check_path(path: &str) -> Result<(), String> {
    if Path::new(path).is_absolute() {
        Ok(())
    } else {
        Err(format!("Blender's path {path} is not an absolute path"))
    }
}

/// The executable an export would run: `configured` if it is one, else `blender` on `path_var`.
fn program(configured: Option<&str>, path_var: Option<&OsStr>) -> Option<PathBuf> {
    match configured {
        Some(path) => Some(PathBuf::from(path)).filter(|p| is_executable(p)),
        None => std::env::split_paths(path_var?)
            .map(|dir| dir.join("blender"))
            .find(|p| is_executable(p)),
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The version in `blender --version`'s output, "5.2.2" from "Blender 5.2.2 LTS".
pub fn parse_version(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let version = line
            .trim()
            .strip_prefix("Blender ")?
            .split_whitespace()
            .next()?;
        let parts: Vec<&str> = version.split('.').collect();
        let numeric = parts.len() >= 2
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        numeric.then(|| version.to_string())
    })
}

/// Accepts a version whose major.minor is `BLENDER_SERIES` (D29).
pub fn check_series(version: &str) -> Result<(), String> {
    if version.split('.').take(2).eq(BLENDER_SERIES.split('.')) {
        Ok(())
    } else {
        Err(format!(
            "Blender {version} isn't {BLENDER_SERIES}.x, the only version exports accept"
        ))
    }
}

/// The normalization script (C9), embedded so the installed app needs no file beside it (A2).
/// Each job writes it to a temporary folder of its own.
const NORMALIZE_SCRIPT: &str = include_str!("normalize.py");

/// The script's version, the `by` of the step it records (D33). The script declares it as
/// `VERSION`, which a test keeps equal: bump both when its output changes.
pub const NORMALIZE_VERSION: &str = "opendam-normalize/1";

/// How long one Blender job may run (D30).
pub const JOB_TIMEOUT: Duration = Duration::from_secs(120);

/// The pivot rule's tolerance: `min.y` and the X and Z centre within 0.1% of the largest
/// dimension from 0 (C9).
const PIVOT_TOLERANCE: f64 = 0.001;

/// How many Blender jobs run at once: a quarter of the cores, at least one (D30).
pub fn job_limit() -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    (cores / 4).max(1)
}

/// What a job normalizes. Animations come with plan step 7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    Model,
}

/// One Blender job, the JSON the script reads (C9).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    /// Names the asset in errors, as the Check's problems do. Not sent.
    #[serde(skip)]
    pub asset: String,
    pub kind: JobKind,
    pub input: PathBuf,
    pub output: PathBuf,
    /// The pack's origin, whose default facing comes first (D13): unreal turns +X to +Z
    pub origin_facing: Origin,
    /// Your turns about Unity's Y, 0 to 3, each a quarter turn clockwise seen from above, which
    /// takes +Z to +X
    pub facing_quarter_turns: u8,
    /// `collision::PATTERNS`
    pub patterns: &'static Patterns,
    /// The material a kept mesh with none gets (`names::added_material`). The script refuses
    /// the job when a material already holds it.
    pub added_material: String,
}

/// What the script wrote, re-imported (C9 step 6), with what it read from the source. The export
/// cache keeps it beside the FBX (C11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Normalized {
    pub bounds: Bounds,
    /// The written meshes, by name
    pub meshes: Vec<String>,
    /// The written material slots, in Blender's order: each kept mesh's by name, then by slot
    pub slots: Vec<String>,
    /// The written file's takes that animate something, sorted, a name written twice listed
    /// twice
    pub takes: Vec<String>,
    pub bones: Vec<String>,
    pub has_mesh: bool,
    pub has_armature: bool,
    /// `NORMALIZE_VERSION`, as the script declares it
    pub version: String,
    /// The exact Blender version, "5.2.2", the step's `toolVersion`
    pub blender_version: String,
    /// The collision meshes it dropped, by name
    pub dropped_meshes: Vec<String>,
    /// The job's `added_material`, when a kept mesh had none
    pub added_material: Option<String>,
    pub transform: Transform,
    pub source: Source,
}

/// Bounds in Unity's axes, in metres, as the manifest's `bounds`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

/// The root transform the script applied (C9 step 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transform {
    pub origin_facing: Origin,
    pub facing_quarter_turns: u8,
    /// The whole turn about Unity's Y, in degrees clockwise seen from above, 0 to 270
    pub yaw: u16,
    /// The translation after the turn, in Unity's axes, in metres
    pub pivot: [f64; 3],
}

/// The source's kept slots and takes, read before any change (C9 step 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub slots: Vec<String>,
    /// The source file's takes that animate something it keeps, sorted, read from its node
    /// tree as the preview worker's `fbx_facts` reads them, not from Blender's import
    pub takes: Vec<String>,
}

/// The script's result file: the report, or why it refused.
pub fn parse_result(text: &str) -> Result<Normalized, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("Blender's result isn't JSON: {e}"))?;
    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let error = value.get("error").and_then(serde_json::Value::as_str);
        return Err(error.unwrap_or("Blender gave no reason").to_string());
    }
    serde_json::from_value(value).map_err(|e| format!("Blender's result is malformed: {e}"))
}

/// Checks what the script wrote (C9): the pivot rule, and that the written slots and takes are
/// the source's kept ones, plus the added material.
pub fn check(job: &Job, normalized: &Normalized) -> Result<(), String> {
    let Bounds { min, max } = normalized.bounds;
    let largest = (0..3).map(|i| max[i] - min[i]).fold(0.0, f64::max);
    if largest <= 0.0 {
        return Err("the normalized model has no size".into());
    }
    let tolerance = largest * PIVOT_TOLERANCE;
    for (what, offset) in [
        ("min.y", min[1]),
        ("the X centre", (min[0] + max[0]) / 2.0),
        ("the Z centre", (min[2] + max[2]) / 2.0),
    ] {
        if offset.abs() > tolerance {
            return Err(format!(
                "the normalized pivot is off: {what} is {offset} m from 0, more than 0.1% of \
                 the largest dimension, {largest} m"
            ));
        }
    }

    let added = normalized.added_material.as_deref();
    if added.is_some_and(|name| name != job.added_material) {
        return Err(format!(
            "Blender added the material {}, not {}",
            list(added),
            job.added_material
        ));
    }
    let kept: Vec<&str> = normalized
        .slots
        .iter()
        .map(String::as_str)
        .filter(|slot| Some(*slot) != added)
        .collect();
    let added_once =
        added.is_none_or(|name| normalized.slots.iter().filter(|s| *s == name).count() == 1);
    if kept != normalized.source.slots || !added_once {
        return Err(format!(
            "Blender's round trip changed the material slots: the source keeps {}{}, and it \
             wrote {}",
            list(&normalized.source.slots),
            added.map_or(String::new(), |name| format!(" (and {name} was added)")),
            list(&normalized.slots),
        ));
    }
    if normalized.takes != normalized.source.takes {
        return Err(format!(
            "Blender's round trip changed the takes: the source keeps {}, and it wrote {}",
            list(&normalized.source.takes),
            list(&normalized.takes),
        ));
    }
    Ok(())
}

/// Checks the source's kept slots and takes, as Blender read them, against the preview
/// worker's `fbx_facts`, which the Check showed and your edits name (C9). They are compared as
/// sets, since Blender and the worker may order them differently: a difference means the two
/// read the model differently, and the manifest would list materials or clips the Check never
/// showed. The export calls it with the facts its Check read.
pub fn check_facts(normalized: &Normalized, facts: &FbxFacts) -> Result<(), String> {
    use std::collections::BTreeSet;
    let blender: BTreeSet<&str> = normalized.source.slots.iter().map(String::as_str).collect();
    let worker: BTreeSet<&str> = facts.slots.iter().map(|s| s.name.as_str()).collect();
    if blender != worker {
        return Err(format!(
            "Blender and the preview engine read different material slots: Blender keeps {}, \
             and the preview engine {}",
            list(&blender),
            list(&worker),
        ));
    }
    let blender: BTreeSet<&str> = normalized.source.takes.iter().map(String::as_str).collect();
    let worker: BTreeSet<&str> = facts.takes.iter().map(String::as_str).collect();
    if blender != worker {
        return Err(format!(
            "Blender and the preview engine read different takes: Blender keeps {}, and the \
             preview engine {}",
            list(&blender),
            list(&worker),
        ));
    }
    Ok(())
}

/// The conversion step a normalized model records (C9, D33): Blender at the exact version that
/// wrote the file, by `NORMALIZE_VERSION`, with flat settings: your quarter turns, the origin
/// whose facing came first, the pivot's translation, how many collision meshes were dropped, and
/// the added material's name, when one was added.
pub fn step(normalized: &Normalized) -> serde_json::Value {
    let transform = &normalized.transform;
    let [x, y, z] = transform.pivot;
    let mut settings = serde_json::json!({
        "facingQuarterTurns": transform.facing_quarter_turns,
        "originFacing": transform.origin_facing,
        "pivotX": x,
        "pivotY": y,
        "pivotZ": z,
        "droppedMeshes": normalized.dropped_meshes.len(),
    });
    if let Some(added) = &normalized.added_material {
        settings["addedMaterial"] = added.as_str().into();
    }
    serde_json::json!({
        "tool": "Blender",
        "toolVersion": normalized.blender_version,
        "by": NORMALIZE_VERSION,
        "from": "fbx",
        "settings": settings,
    })
}

/// Names joined for a message, or "none".
fn list<S: AsRef<str>>(names: impl IntoIterator<Item = S>) -> String {
    let names: Vec<String> = names.into_iter().map(|n| n.as_ref().to_string()).collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

/// Runs Blender jobs, at most `job_limit` at a time, each within `JOB_TIMEOUT` (D30).
pub struct Runner {
    blender: Blender,
    permits: Semaphore,
    timeout: Duration,
}

impl Runner {
    pub fn new(blender: Blender) -> Self {
        Runner::with_limits(blender, job_limit(), JOB_TIMEOUT)
    }

    fn with_limits(blender: Blender, jobs: usize, timeout: Duration) -> Self {
        Runner {
            blender,
            permits: Semaphore::new(jobs),
            timeout,
        }
    }

    pub fn blender(&self) -> &Blender {
        &self.blender
    }

    /// Normalizes `job`'s FBX and checks what it wrote. Every error names the asset. Only a
    /// checked FBX reaches `job.output`: a failed job leaves it as it was.
    pub async fn run(&self, job: &Job) -> Result<Normalized, String> {
        self.normalize(job)
            .await
            .map_err(|e| format!("{}: {e}", job.asset))
    }

    /// Runs `<blender> -b --factory-startup -P <script> -- <job.json> <result.json>`, with the
    /// script writing into the job's temporary folder, then checks the result and publishes
    /// the FBX. Its file work runs off the async threads, since the FBX can be large.
    async fn normalize(&self, job: &Job) -> Result<Normalized, String> {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| "the Blender jobs were stopped".to_string())?;
        let dir = tokio::task::spawn_blocking(|| {
            tempfile::Builder::new()
                .prefix("opendam-normalize-")
                .tempdir()
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("Could not make a folder for Blender's job: {e}"))?;
        let normalized = self.normalize_in(dir.path(), job).await;
        // Removing the folder deletes the staged FBX too
        let _ = tokio::task::spawn_blocking(move || drop(dir)).await;
        normalized
    }

    async fn normalize_in(&self, dir: &Path, job: &Job) -> Result<Normalized, String> {
        let script = dir.join("normalize.py");
        let job_file = dir.join("job.json");
        let result_file = dir.join("result.json");
        let staged = dir.join("output.fbx");
        let sent = Job {
            output: staged.clone(),
            ..job.clone()
        };
        let job_json = serde_json::to_vec(&sent).map_err(|e| e.to_string())?;
        let written = match tokio::fs::write(&script, NORMALIZE_SCRIPT).await {
            Ok(()) => tokio::fs::write(&job_file, job_json).await,
            error => error,
        };
        written.map_err(|e| format!("Could not write Blender's job: {e}"))?;

        let child = tokio::process::Command::new(&self.blender.program)
            .args(["-b", "--factory-startup", "-P"])
            .arg(&script)
            .arg("--")
            .arg(&job_file)
            .arg(&result_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Could not start Blender: {e}"))?;
        // Dropping the child on a timeout kills it
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| {
                format!(
                    "Blender didn't finish normalizing it in {} s",
                    self.timeout.as_secs()
                )
            })?
            .map_err(|e| format!("Blender's job failed: {e}"))?;
        let normalized = match tokio::fs::read_to_string(&result_file).await {
            Ok(text) if output.status.success() => parse_result(&text)?,
            _ => return Err(crashed(&output)),
        };
        check(job, &normalized)?;
        publish(&staged, &job.output).await?;
        Ok(normalized)
    }
}

/// Puts the checked FBX at `staged` at `output`: copied beside it, then renamed over it, so
/// `output` is only ever the old file or the whole new one.
/// The staged FBX sits in the system's temporary folder, often another filesystem, so it is
/// copied rather than renamed there.
async fn publish(staged: &Path, output: &Path) -> Result<(), String> {
    let name = output
        .file_name()
        .ok_or("the job's output has no file name")?
        .to_string_lossy();
    let partial = output.with_file_name(format!(".{name}.{}.partial", uuid::Uuid::new_v4()));
    let written = async {
        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::copy(staged, &partial).await?;
        tokio::fs::rename(&partial, output).await
    }
    .await;
    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(format!("Could not write {}: {e}", output.display()));
    }
    Ok(())
}

/// Why a Blender job left no result: its status and the last lines it printed.
fn crashed(output: &std::process::Output) -> String {
    let printed = [&output.stderr, &output.stdout]
        .into_iter()
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .find(|text| !text.trim().is_empty())
        .unwrap_or_default();
    let lines: Vec<&str> = printed.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(3)..].join(" / ");
    let status = if output.status.success() {
        "Blender exited without a result".to_string()
    } else {
        format!("Blender crashed ({})", output.status)
    };
    if tail.is_empty() {
        status
    } else {
        format!("{status}: {tail}")
    }
}

/// The Blender in `OPENDAM_BLENDER`, for tests that run it. Without it, a test skips with a
/// message, except under `CI`, which installs it (`.github/blender.env`): there it fails.
#[cfg(test)]
pub(crate) fn test_blender() -> Option<PathBuf> {
    let found = std::env::var_os("OPENDAM_BLENDER")
        .map(PathBuf::from)
        .filter(|p| is_executable(p));
    if found.is_none() {
        let message = "OPENDAM_BLENDER doesn't name an executable Blender";
        if std::env::var_os("CI").is_some_and(|v| !v.is_empty()) {
            panic!("{message}, and CI must run the Blender-backed tests");
        }
        eprintln!("Skipping: {message}. Set it to a Blender {BLENDER_SERIES}.x to run this test.");
    }
    found
}

/// What the script reports for a model, for tests that need a result without Blender: a 1 m
/// cube standing on its pivot, unturned, made by Blender 5.2.2 from a source keeping `slots` and
/// `takes`, written with `added` after the slots, and an armature when it has `bones`.
#[cfg(test)]
pub(crate) fn test_normalized(
    slots: &[&str],
    added: Option<&str>,
    takes: &[&str],
    bones: &[&str],
) -> Normalized {
    let written: Vec<&str> = slots.iter().copied().chain(added).collect();
    serde_json::from_value(serde_json::json!({
        "bounds": {"min": [-0.5, 0.0, -0.5], "max": [0.5, 1.0, 0.5]},
        "meshes": ["Mesh"],
        "slots": written,
        "takes": takes,
        "bones": bones,
        "hasMesh": true,
        "hasArmature": !bones.is_empty(),
        "version": NORMALIZE_VERSION,
        "blenderVersion": "5.2.2",
        "droppedMeshes": [],
        "addedMaterial": added,
        "transform": {
            "originFacing": "generic",
            "facingQuarterTurns": 0,
            "yaw": 0,
            "pivot": [0.0, 0.25, 0.0],
        },
        "source": {"slots": slots, "takes": takes},
    }))
    .expect("a result")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake `blender` that prints `stdout` for `--version`.
    fn stub(dir: &Path, stdout: &str) -> PathBuf {
        let path = dir.join("blender");
        std::fs::write(&path, format!("#!/bin/sh\nprintf '{stdout}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn configured(path: &Path) -> Option<&str> {
        path.to_str()
    }

    #[test]
    fn versions_are_read_from_blender_s_banner() {
        for (output, version) in [
            (
                "Blender 5.2.2 LTS\n\tbuild date: 2026-09-22\n",
                Some("5.2.2"),
            ),
            ("Blender 5.3.0 Alpha\n", Some("5.3.0")),
            ("Blender 5.2\n", Some("5.2")),
            ("Read prefs: ...\nBlender 4.5.1\n", Some("4.5.1")),
            ("Blender 5\n", None),
            ("Blender five.two\n", None),
            ("Blender 5..2\n", None),
            ("blender 5.2.2\n", None),
            ("", None),
        ] {
            assert_eq!(parse_version(output).as_deref(), version, "{output:?}");
        }
    }

    #[test]
    fn the_gate_accepts_only_the_pinned_major_minor() {
        for version in ["5.2", "5.2.0", "5.2.2", "5.2.10"] {
            assert_eq!(check_series(version), Ok(()), "{version}");
        }
        for version in ["5.1.1", "5.3.0", "5.20.1", "4.2.3", "6.2.0", "5"] {
            let error = check_series(version).unwrap_err();
            assert!(error.contains("isn't 5.2.x"), "{version}: {error}");
        }
    }

    #[tokio::test]
    async fn a_configured_blender_of_the_series_passes_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let blender = stub(dir.path(), "Blender 5.2.2 LTS\\n");

        let gated = gate(configured(&blender)).await.unwrap();

        assert_eq!(
            gated,
            Blender {
                program: blender.clone(),
                version: "5.2.2".into()
            }
        );
        assert_eq!(
            status(configured(&blender)).await,
            BlenderStatus {
                series: BLENDER_SERIES,
                path: Some(blender.display().to_string()),
                version: Some("5.2.2".into()),
                accepted: true,
                problem: None,
            }
        );
    }

    #[tokio::test]
    async fn another_series_is_found_but_refused() {
        let dir = tempfile::tempdir().unwrap();
        let blender = stub(dir.path(), "Blender 4.5.3 LTS\\n");

        let error = gate(configured(&blender)).await.unwrap_err();
        let status = status(configured(&blender)).await;

        assert!(error.contains("Blender 4.5.3 isn't 5.2.x"), "{error}");
        assert_eq!(status.series, BLENDER_SERIES);
        assert_eq!(status.version.as_deref(), Some("4.5.3"));
        assert!(!status.accepted);
        assert_eq!(status.problem, Some(error));
    }

    #[tokio::test]
    async fn a_missing_or_silent_blender_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope/blender");
        let not_executable = dir.path().join("blender.txt");
        std::fs::write(&not_executable, "").unwrap();
        let silent = stub(dir.path(), "not blender\\n");

        for (path, why) in [
            (&missing, "No Blender at"),
            (&not_executable, "No Blender at"),
            (&silent, "didn't report a Blender version"),
        ] {
            let error = gate(configured(path)).await.unwrap_err();
            assert!(error.contains(why), "{error}");
            let status = status(configured(path)).await;
            assert!(!status.accepted);
            assert_eq!(status.version, None);
            assert_eq!(status.problem, Some(error));
        }
    }

    #[tokio::test]
    async fn a_relative_path_is_refused_even_when_it_reaches_a_blender() {
        let dir = tempfile::tempdir().unwrap();
        let blender = stub(dir.path(), "Blender 5.2.2\\n");
        // The same stub, reached from the working directory through `..`
        let cwd = std::env::current_dir().unwrap();
        let up = "../".repeat(cwd.components().count() - 1);
        let relative = format!("{up}{}", blender.strip_prefix("/").unwrap().display());
        assert!(Path::new(&relative).is_file(), "{relative}");

        let error = gate(Some(&relative)).await.unwrap_err();
        let status = status(Some(&relative)).await;

        assert!(error.contains("is not an absolute path"), "{error}");
        assert_eq!(status.path, None);
        assert_eq!(status.version, None);
        assert!(!status.accepted);
        assert_eq!(status.problem, Some(error));
    }

    #[test]
    fn an_unset_path_finds_blender_on_path() {
        let empty = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let blender = stub(dir.path(), "Blender 5.2.2\\n");
        let path_var = std::env::join_paths([empty.path(), dir.path()]).unwrap();

        assert_eq!(program(None, Some(&path_var)), Some(blender));
        assert_eq!(program(None, Some(empty.path().as_os_str())), None);
        assert_eq!(program(None, None), None);
    }

    /// CI's pin and `BLENDER_SERIES` move together, so CI always tests the Blender exports accept.
    #[test]
    fn ci_pins_a_blender_of_the_accepted_series() {
        let pin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.github/blender.env");
        let text = std::fs::read_to_string(&pin).unwrap();
        let version = text
            .lines()
            .find_map(|line| line.strip_prefix("BLENDER_VERSION="))
            .expect("blender.env sets BLENDER_VERSION");

        assert!(
            version.starts_with(&format!("{BLENDER_SERIES}.")),
            "CI pins Blender {version}, but exports accept only {BLENDER_SERIES}.x"
        );
    }

    /// Runs the Blender CI installs headless once, so CI proves the install works.
    #[tokio::test]
    async fn blender_runs_headless() {
        let Some(blender) = test_blender() else {
            return;
        };

        let gated = gate(blender.to_str()).await.unwrap();
        let output = tokio::process::Command::new(&gated.program)
            .args(["-b", "--factory-startup", "--python-exit-code", "1"])
            .args([
                "--python-expr",
                "import bpy; print('OPENDAM_OK', bpy.app.version_string)",
            ])
            .output()
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);

        assert!(output.status.success(), "{stdout}");
        assert!(
            stdout.contains(&format!("OPENDAM_OK {}", gated.version)),
            "{stdout}"
        );
    }

    #[test]
    fn the_script_declares_the_version_rust_records() {
        let line = format!("VERSION = \"{NORMALIZE_VERSION}\"");
        assert!(
            NORMALIZE_SCRIPT.lines().any(|l| l == line),
            "normalize.py must declare {line}"
        );
        assert!(NORMALIZE_VERSION.starts_with("opendam-normalize/"));
    }

    #[test]
    fn a_quarter_of_the_cores_run_at_once() {
        let cores = std::thread::available_parallelism().unwrap().get();
        assert_eq!(job_limit(), (cores / 4).max(1));
        assert!(job_limit() >= 1);
    }

    fn lamp_job() -> Job {
        Job {
            asset: "Dungeon Props/Models/SM_Lamp.fbx".into(),
            kind: JobKind::Model,
            input: "/lib/Dungeon Props/Models/SM_Lamp.fbx".into(),
            output: "/cache/SM_Lamp.fbx".into(),
            origin_facing: Origin::Unreal,
            facing_quarter_turns: 1,
            patterns: &crate::export::collision::PATTERNS,
            added_material: "OpenDAMDefault".into(),
        }
    }

    #[test]
    fn the_job_is_the_json_the_script_reads() {
        assert_eq!(
            serde_json::to_value(lamp_job()).unwrap(),
            serde_json::json!({
                "kind": "model",
                "input": "/lib/Dungeon Props/Models/SM_Lamp.fbx",
                "output": "/cache/SM_Lamp.fbx",
                "originFacing": "unreal",
                "facingQuarterTurns": 1,
                "patterns": {
                    "prefixes": ["UCX_", "UBX_", "USP_", "UCP_"],
                    "suffixes": ["_Collision", "_COL"],
                },
                "addedMaterial": "OpenDAMDefault",
            })
        );
    }

    /// The script's report for `SM_Lamp`, with a real `DefaultMaterial` and an added slot.
    fn lamp_result() -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "bounds": {"min": [-0.2, 0.0, -0.2], "max": [0.2, 0.7, 0.2]},
            "meshes": ["SM_Lamp_Base", "SM_Lamp_Shade"],
            "slots": ["DefaultMaterial", "OpenDAMDefault"],
            "takes": [],
            "bones": [],
            "hasMesh": true,
            "hasArmature": false,
            "version": NORMALIZE_VERSION,
            "blenderVersion": "5.2.2",
            "droppedMeshes": [],
            "addedMaterial": "OpenDAMDefault",
            "transform": {
                "originFacing": "unreal",
                "facingQuarterTurns": 1,
                "yaw": 0,
                "pivot": [0.0, 0.0, -0.05],
            },
            "source": {"slots": ["DefaultMaterial"], "takes": []},
        })
    }

    fn lamp() -> Normalized {
        parse_result(&lamp_result().to_string()).unwrap()
    }

    #[test]
    fn results_parse_into_the_report_or_why_it_refused() {
        let normalized = lamp();
        assert_eq!(
            normalized.bounds,
            Bounds {
                min: [-0.2, 0.0, -0.2],
                max: [0.2, 0.7, 0.2]
            }
        );
        assert_eq!(normalized.added_material.as_deref(), Some("OpenDAMDefault"));
        assert_eq!(normalized.transform.origin_facing, Origin::Unreal);
        assert_eq!(normalized.transform.pivot, [0.0, 0.0, -0.05]);
        assert_eq!(normalized.source.slots, ["DefaultMaterial"]);

        let refused = r#"{"ok": false, "error": "it has no mesh to keep"}"#;
        assert_eq!(parse_result(refused), Err("it has no mesh to keep".into()));
        assert_eq!(
            parse_result(r#"{"ok": false}"#),
            Err("Blender gave no reason".into())
        );
        assert!(parse_result("{")
            .unwrap_err()
            .starts_with("Blender's result isn't JSON"));
        assert!(parse_result(r#"{"ok": true, "bounds": null}"#)
            .unwrap_err()
            .starts_with("Blender's result is malformed"));
    }

    #[test]
    fn the_pivot_rule_allows_a_tenth_of_a_percent_of_the_largest_dimension() {
        let job = lamp_job();
        assert_eq!(check(&job, &lamp()), Ok(()));
        // 0.7 m tall: 0.0007 m is the most any of the three may be off
        for (min, max, ok) in [
            ([-0.2, 0.0006, -0.2], [0.2, 0.7, 0.2], true),
            ([-0.2, 0.0008, -0.2], [0.2, 0.7, 0.2], false),
            ([-0.2, -0.0008, -0.2], [0.2, 0.7, 0.2], false),
            ([-0.1988, 0.0, -0.2], [0.2, 0.7, 0.2], true),
            ([-0.1984, 0.0, -0.2], [0.2, 0.7, 0.2], false),
            ([-0.2, 0.0, -0.2], [0.2, 0.7, 0.2016], false),
        ] {
            let mut normalized = lamp();
            normalized.bounds = Bounds { min, max };
            let checked = check(&job, &normalized);
            assert_eq!(checked.is_ok(), ok, "{min:?} {max:?}: {checked:?}");
            if !ok {
                assert!(
                    checked
                        .unwrap_err()
                        .starts_with("the normalized pivot is off"),
                    "{min:?}"
                );
            }
        }
        let mut flat = lamp();
        flat.bounds = Bounds {
            min: [0.0; 3],
            max: [0.0; 3],
        };
        assert_eq!(
            check(&job, &flat),
            Err("the normalized model has no size".into())
        );
    }

    #[test]
    fn written_slots_must_be_the_source_s_kept_ones_plus_the_added_material() {
        let job = lamp_job();
        let with = |slots: &[&str], added: Option<&str>| {
            let mut normalized = lamp();
            normalized.slots = slots.iter().map(|s| s.to_string()).collect();
            normalized.added_material = added.map(str::to_string);
            check(&job, &normalized)
        };

        // The added material may sort anywhere among the source's
        assert_eq!(
            with(
                &["OpenDAMDefault", "DefaultMaterial"],
                Some("OpenDAMDefault")
            ),
            Ok(())
        );
        assert_eq!(
            with(
                &["DefaultMaterial.001", "OpenDAMDefault"],
                Some("OpenDAMDefault")
            ),
            Err(
                "Blender's round trip changed the material slots: the source keeps \
                 DefaultMaterial (and OpenDAMDefault was added), and it wrote \
                 DefaultMaterial.001, OpenDAMDefault"
                    .into()
            )
        );
        // Added but not written, or written twice
        assert!(with(&["DefaultMaterial"], Some("OpenDAMDefault")).is_err());
        assert!(with(
            &["DefaultMaterial", "OpenDAMDefault", "OpenDAMDefault"],
            Some("OpenDAMDefault")
        )
        .is_err());
        // Written though the script says nothing was added
        assert_eq!(
            with(&["DefaultMaterial", "OpenDAMDefault"], None),
            Err(
                "Blender's round trip changed the material slots: the source keeps \
                 DefaultMaterial, and it wrote DefaultMaterial, OpenDAMDefault"
                    .into()
            )
        );
        // Another name than the job's
        assert_eq!(
            with(
                &["DefaultMaterial", "OpenDAMDefault2"],
                Some("OpenDAMDefault2")
            ),
            Err("Blender added the material OpenDAMDefault2, not OpenDAMDefault".into())
        );
    }

    #[test]
    fn written_takes_must_be_the_source_s_kept_ones() {
        let job = lamp_job();
        let mut normalized = lamp();
        normalized.source.takes = vec!["Idle".into(), "Wave".into()];
        normalized.takes = vec!["Idle".into(), "Wave".into()];
        assert_eq!(check(&job, &normalized), Ok(()));
        normalized.takes = vec!["Armature|Wave".into(), "Idle".into()];
        assert_eq!(
            check(&job, &normalized),
            Err(
                "Blender's round trip changed the takes: the source keeps Idle, Wave, and it \
                 wrote Armature|Wave, Idle"
                    .into()
            )
        );
        normalized.takes = vec![];
        assert!(check(&job, &normalized)
            .unwrap_err()
            .ends_with("it wrote none"));
        // A take written twice, as a take split over two objects would be
        normalized.takes = vec!["Idle".into(), "Wave".into(), "Wave".into()];
        assert_eq!(
            check(&job, &normalized),
            Err(
                "Blender's round trip changed the takes: the source keeps Idle, Wave, and it \
                 wrote Idle, Wave, Wave"
                    .into()
            )
        );
    }

    #[test]
    fn the_source_must_read_as_the_worker_read_it_in_any_order() {
        let facts = |slots: &[&str], takes: &[&str]| FbxFacts {
            slots: slots
                .iter()
                .map(|name| crate::export::fbx_facts::Slot {
                    name: name.to_string(),
                    ..Default::default()
                })
                .collect(),
            takes: takes.iter().map(|t| t.to_string()).collect(),
            ..FbxFacts::default()
        };
        let mut normalized = lamp();
        normalized.source.slots = vec!["Stone".into(), "Moss".into()];
        normalized.source.takes = vec!["Idle".into(), "Wave".into()];

        assert_eq!(
            check_facts(&normalized, &facts(&["Moss", "Stone"], &["Wave", "Idle"])),
            Ok(())
        );
        assert_eq!(
            check_facts(&normalized, &facts(&["Stone"], &["Idle", "Wave"])),
            Err(
                "Blender and the preview engine read different material slots: Blender keeps \
                 Moss, Stone, and the preview engine Stone"
                    .into()
            )
        );
        assert_eq!(
            check_facts(&normalized, &facts(&["Stone", "Moss"], &["Idle"])),
            Err(
                "Blender and the preview engine read different takes: Blender keeps Idle, \
                 Wave, and the preview engine Idle"
                    .into()
            )
        );
    }

    /// A fake Blender running the shell `body`, which gets the job's arguments as `$1`..`$7`.
    fn fake(dir: &Path, body: &str) -> Blender {
        let program = dir.join("blender");
        std::fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        Blender {
            program,
            version: "5.2.2".into(),
        }
    }

    /// The shell line with which a fake Blender writes its FBX, where the job tells the script
    /// to: `output.fbx` beside the result.
    const WRITES_FBX: &str = "printf 'FBX' > \"$(dirname \"$7\")/output.fbx\"";

    /// A fake Blender that keeps its arguments, script and job in `dir`, writes its FBX, then
    /// answers `result`.
    fn answering(dir: &Path, result: &serde_json::Value) -> Blender {
        let answer = dir.join("answer.json");
        std::fs::write(&answer, result.to_string()).unwrap();
        let d = dir.display();
        fake(
            dir,
            &format!(
                "printf '%s\\n' \"$@\" > '{d}/argv'\ncp \"$4\" '{d}/script.py'\n\
                 cp \"$6\" '{d}/job.json'\n{WRITES_FBX}\ncp '{d}/answer.json' \"$7\""
            ),
        )
    }

    /// `lamp_job`, writing into `dir`.
    fn lamp_job_in(dir: &Path) -> Job {
        Job {
            output: dir.join("cache/SM_Lamp.fbx"),
            ..lamp_job()
        }
    }

    #[tokio::test]
    async fn a_job_runs_the_embedded_script_with_its_job_and_result_files() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner::new(answering(dir.path(), &lamp_result()));
        let job = lamp_job_in(dir.path());

        let normalized = runner.run(&job).await.unwrap();

        assert_eq!(normalized, lamp());
        // The checked FBX is published at the job's output, and nothing else is left there
        assert_eq!(std::fs::read_to_string(&job.output).unwrap(), "FBX");
        let cache: Vec<_> = std::fs::read_dir(dir.path().join("cache"))
            .unwrap()
            .collect();
        assert_eq!(cache.len(), 1);
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        let argv: Vec<&str> = argv.lines().collect();
        assert_eq!(argv[..3], ["-b", "--factory-startup", "-P"]);
        assert!(argv[3].ends_with("/normalize.py"), "{argv:?}");
        assert_eq!(argv[4], "--");
        assert!(argv[5].ends_with("/job.json"), "{argv:?}");
        assert!(argv[6].ends_with("/result.json"), "{argv:?}");
        assert_eq!(argv.len(), 7);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("script.py")).unwrap(),
            NORMALIZE_SCRIPT
        );
        // The script writes into the job's temporary folder, not the output
        let sent: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("job.json")).unwrap()).unwrap();
        let staged = Path::new(argv[6]).with_file_name("output.fbx");
        let mut expected = serde_json::to_value(&job).unwrap();
        expected["output"] = staged.to_str().unwrap().into();
        assert_eq!(sent, expected);
        // The job's temporary folder is gone
        assert!(!Path::new(argv[3]).exists());
    }

    #[tokio::test]
    async fn every_failure_names_the_asset() {
        let asset = "Dungeon Props/Models/SM_Lamp.fbx";
        let refused = serde_json::json!({"ok": false, "error": "it has no mesh to keep"});
        let mut off = lamp_result();
        off["bounds"]["min"][1] = 0.1.into();
        // Each fake writes its FBX before it fails, as a Blender that crashes after exporting
        for (body, error) in [
            (
                "echo 'Fatal Python error' >&2\necho 'Segmentation fault' >&2\nexit 139",
                "Blender crashed (exit status: 139): Fatal Python error / Segmentation fault"
                    .to_string(),
            ),
            ("exit 0", "Blender exited without a result".into()),
            ("exit 3", "Blender crashed (exit status: 3)".into()),
            ("echo '{' > \"$7\"", "Blender's result isn't JSON".into()),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let runner = Runner::new(fake(dir.path(), &format!("{WRITES_FBX}\n{body}")));
            let job = lamp_job_in(dir.path());
            let message = runner.run(&job).await.unwrap_err();
            assert!(
                message.starts_with(&format!("{asset}: {error}")),
                "{message}"
            );
            assert!(!job.output.exists(), "{body}");
        }
        for (result, error) in [
            (&refused, "it has no mesh to keep"),
            (&off, "the normalized pivot is off: min.y is 0.1 m from 0"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let runner = Runner::new(answering(dir.path(), result));
            let job = lamp_job_in(dir.path());
            let message = runner.run(&job).await.unwrap_err();
            assert!(
                message.starts_with(&format!("{asset}: {error}")),
                "{message}"
            );
            // A refused or rejected FBX never reaches the output
            assert!(!job.output.exists(), "{error}");
        }

        // An output that can't be written: no partial file is left beside it
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner::new(answering(dir.path(), &lamp_result()));
        std::fs::write(dir.path().join("cache"), "a file, not a folder").unwrap();
        let job = lamp_job_in(dir.path());
        let message = runner.run(&job).await.unwrap_err();
        assert!(
            message.starts_with(&format!(
                "{asset}: Could not write {}",
                job.output.display()
            )),
            "{message}"
        );
        let blocked = Job {
            output: dir.path().join("SM_Lamp.fbx/"),
            ..lamp_job()
        };
        std::fs::create_dir(&blocked.output).unwrap();
        assert!(runner.run(&blocked).await.is_err());
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".partial"))
            .collect();
        assert_eq!(left, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_job_past_its_timeout_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let runner = Runner::with_limits(
            fake(
                dir.path(),
                &format!(
                    "{WRITES_FBX}\necho $$ > '{}'\nexec sleep 30",
                    pid_file.display()
                ),
            ),
            1,
            Duration::from_secs(1),
        );
        let job = lamp_job_in(dir.path());
        let started = std::time::Instant::now();

        let message = runner.run(&job).await.unwrap_err();

        assert_eq!(
            message,
            "Dungeon Props/Models/SM_Lamp.fbx: Blender didn't finish normalizing it in 1 s"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(!job.output.exists());
        // The process is killed, not left running: gone, or a zombie until it is reaped
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let stat = format!("/proc/{}/stat", pid.trim());
        let alive = || {
            std::fs::read_to_string(&stat)
                .is_ok_and(|s| s.rsplit(") ").next().is_some_and(|s| !s.starts_with('Z')))
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !alive(),
            "Blender {} still runs after its timeout",
            pid.trim()
        );
    }

    #[tokio::test]
    async fn no_more_jobs_than_the_limit_run_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let answer = dir.path().join("answer.json");
        std::fs::write(&answer, lamp_result().to_string()).unwrap();
        let running = dir.path().join("running");
        std::fs::create_dir(&running).unwrap();
        let (r, d) = (running.display(), dir.path().display());
        let blender = fake(
            dir.path(),
            &format!(
                "touch '{r}'/$$\nls '{r}' | wc -l >> '{d}/counts'\nsleep 1\nrm '{r}'/$$\n\
                 {WRITES_FBX}\ncp '{d}/answer.json' \"$7\""
            ),
        );
        let runner = Runner::with_limits(blender, 2, JOB_TIMEOUT);
        let jobs: Vec<Job> = (0..4)
            .map(|n| Job {
                output: dir.path().join(format!("SM_Lamp{n}.fbx")),
                ..lamp_job()
            })
            .collect();

        let results = futures_util::future::join_all(jobs.iter().map(|job| runner.run(job))).await;

        assert!(results.iter().all(Result::is_ok), "{results:?}");
        let counts = std::fs::read_to_string(dir.path().join("counts")).unwrap();
        let counts: Vec<usize> = counts.lines().map(|l| l.trim().parse().unwrap()).collect();
        assert_eq!(counts.len(), 4);
        assert_eq!(counts.iter().max(), Some(&2), "{counts:?}");
    }

    const LIBRARY: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/game-export/library"
    );

    /// A runner of the Blender in `OPENDAM_BLENDER`, or `None` to skip.
    async fn fixture_runner() -> Option<Runner> {
        let program = test_blender()?;
        Some(Runner::new(gate(program.to_str()).await.unwrap()))
    }

    /// The fixture model `name`'s facts, as the preview worker reads them.
    fn fixture_facts(name: &str) -> crate::export::fbx_facts::FbxFacts {
        let mut facts: std::collections::HashMap<String, crate::export::fbx_facts::FbxFacts> =
            serde_json::from_str(include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/game-export/fbx_facts.json"
            )))
            .unwrap();
        facts
            .remove(&format!("Dungeon Props/Models/{name}.fbx"))
            .unwrap()
    }

    /// The job for the fixture model `name`, written into `out`, with the added material's name
    /// picked from its facts as the export picks it.
    fn fixture_job(name: &str, out: &Path) -> Job {
        let asset = format!("Dungeon Props/Models/{name}.fbx");
        let added = crate::export::names::added_material(
            fixture_facts(name).slots.iter().map(|s| s.name.as_str()),
        );
        Job {
            input: Path::new(LIBRARY).join(&asset),
            output: out.join(format!("{name}.fbx")),
            asset,
            kind: JobKind::Model,
            origin_facing: Origin::Generic,
            facing_quarter_turns: 0,
            patterns: &crate::export::collision::PATTERNS,
            added_material: added,
        }
    }

    /// Checks `bounds` is a box of `size` metres (Unity's x, y, z) standing on its bottom
    /// centre, within 0.1% of its largest dimension.
    pub(crate) fn assert_size(bounds: Bounds, size: [f64; 3]) {
        let tolerance = size.iter().cloned().fold(0.0, f64::max) * PIVOT_TOLERANCE;
        let min = [-size[0] / 2.0, 0.0, -size[2] / 2.0];
        let max = [size[0] / 2.0, size[1], size[2] / 2.0];
        for i in 0..3 {
            assert!(
                (bounds.min[i] - min[i]).abs() <= tolerance
                    && (bounds.max[i] - max[i]).abs() <= tolerance,
                "{bounds:?} isn't {min:?}..{max:?}"
            );
        }
    }

    /// The vertices of the FBX at `fbx`, in Unity's axes, as Blender imports them.
    pub(crate) async fn vertices(blender: &Path, fbx: &Path) -> Vec<[f64; 3]> {
        const READ: &str = "import bpy, json, sys
bpy.ops.wm.read_homefile(use_empty=True)
bpy.ops.import_scene.fbx(filepath=sys.argv[-1])
points = [o.matrix_world @ v.co for o in bpy.data.objects if o.type == 'MESH' for v in o.data.vertices]
print('OPENDAM_VERTICES', json.dumps([[-p.x, p.z, -p.y] for p in points]))";
        let output = tokio::process::Command::new(blender)
            .args(["-b", "--factory-startup", "--python-exit-code", "1"])
            .args(["--python-expr", READ, "--"])
            .arg(fbx)
            .output()
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout
            .lines()
            .find_map(|l| l.strip_prefix("OPENDAM_VERTICES "))
            .unwrap_or_else(|| panic!("{stdout}"));
        serde_json::from_str(line).unwrap()
    }

    /// Which way `SM_Totem`'s nose points, in Unity's axes: the side whose outermost vertices
    /// are all the nose's, 1.45 to 1.55 m up.
    pub(crate) fn nose(vertices: &[[f64; 3]]) -> Vec<&'static str> {
        [
            ("+X", 0, 1.0),
            ("-X", 0, -1.0),
            ("+Z", 2, 1.0),
            ("-Z", 2, -1.0),
        ]
        .into_iter()
        .filter(|&(_, axis, sign)| {
            let far = vertices
                .iter()
                .map(|v| v[axis] * sign)
                .fold(f64::MIN, f64::max);
            vertices
                .iter()
                .filter(|v| (v[axis] * sign - far).abs() < 1e-4)
                .all(|v| (1.449..=1.551).contains(&v[1]))
        })
        .map(|(side, ..)| side)
        .collect()
    }

    #[tokio::test]
    async fn a_centimetre_z_up_model_is_normalized_to_metres_facing_plus_z() {
        let Some(runner) = fixture_runner().await else {
            return;
        };
        let out = tempfile::tempdir().unwrap();
        let blender = runner.blender().program.clone();

        let job = fixture_job("SM_Totem", out.path());
        let totem = runner.run(&job).await.unwrap();

        // 40 x 180 x 30 cm in the source, with a 10 cm nose on its front
        assert_size(totem.bounds, [0.4, 1.8, 0.4]);
        assert_eq!(totem.meshes, ["SM_Totem"]);
        assert_eq!(totem.slots, ["Stone"]);
        assert_eq!(totem.version, NORMALIZE_VERSION);
        assert_eq!(totem.blender_version, runner.blender().version);
        assert_eq!(totem.transform.yaw, 0);
        // The source stood 25 cm right of centre (Blender's +X, Unity's -X) and 5 cm deep
        assert_eq!(totem.transform.pivot, [0.25, 0.0, 0.05]);
        assert_eq!(nose(&vertices(&blender, &job.output).await), ["+Z"]);

        // A quarter turn takes +Z to +X, and an unreal origin turns +X to +Z first
        for (origin, turns, yaw, side) in [
            (Origin::Generic, 1, 90, "+X"),
            (Origin::Generic, 2, 180, "-Z"),
            (Origin::Unreal, 0, 270, "-X"),
            (Origin::Unreal, 1, 0, "+Z"),
            (Origin::Unity, 3, 270, "-X"),
        ] {
            let job = Job {
                origin_facing: origin,
                facing_quarter_turns: turns,
                ..fixture_job("SM_Totem", out.path())
            };
            let turned = runner.run(&job).await.unwrap();
            assert_size(turned.bounds, [0.4, 1.8, 0.4]);
            assert_eq!(turned.transform.yaw, yaw, "{origin:?} {turns}");
            assert_eq!(
                nose(&vertices(&blender, &job.output).await),
                [side],
                "{origin:?} {turns}"
            );
        }
    }

    #[tokio::test]
    async fn collision_meshes_are_dropped_and_lods_kept() {
        let Some(runner) = fixture_runner().await else {
            return;
        };
        let out = tempfile::tempdir().unwrap();

        // Its UCX_ hull has a material of its own, which goes with it
        let pillar = runner
            .run(&fixture_job("SM_Pillar", out.path()))
            .await
            .unwrap();
        assert_eq!(pillar.meshes, ["SM_Pillar_LOD0", "SM_Pillar_LOD1"]);
        assert_eq!(pillar.dropped_meshes, ["UCX_SM_Pillar_00"]);
        assert_eq!(pillar.slots, ["Stone", "Moss"]);
        assert_eq!(pillar.added_material, None);
        assert_size(pillar.bounds, [0.6, 3.0, 0.6]);

        // Its hull has no material
        let gate = runner
            .run(&fixture_job("SM_Gate", out.path()))
            .await
            .unwrap();
        assert_eq!(gate.meshes, ["SM_Gate_LOD0", "SM_Gate_LOD1"]);
        assert_eq!(gate.dropped_meshes, ["UCX_SM_Gate_00"]);
        assert_eq!(gate.slots, ["Iron"]);
        assert_eq!(gate.added_material, None);
        assert_size(gate.bounds, [2.0, 2.5, 0.2]);

        // Nothing is left once its hull goes
        let hull = runner.run(&fixture_job("SM_Hull", out.path())).await;
        assert_eq!(
            hull,
            Err("Dungeon Props/Models/SM_Hull.fbx: it has no mesh to keep".into())
        );
    }

    #[tokio::test]
    async fn a_mesh_without_a_material_gets_the_added_one() {
        let Some(runner) = fixture_runner().await else {
            return;
        };
        let out = tempfile::tempdir().unwrap();
        let run = |name: &'static str| {
            let job = fixture_job(name, out.path());
            let runner = &runner;
            async move { runner.run(&job).await.unwrap() }
        };

        // No material at all
        let boulder = run("SM_Boulder").await;
        assert_eq!(boulder.slots, ["OpenDAMDefault"]);
        assert_eq!(boulder.added_material.as_deref(), Some("OpenDAMDefault"));
        assert_eq!(boulder.source.slots, Vec::<String>::new());
        assert_size(boulder.bounds, [1.2, 0.8, 1.0]);

        // A real DefaultMaterial keeps its name beside the added slot
        let lamp = run("SM_Lamp").await;
        assert_eq!(lamp.slots, ["DefaultMaterial", "OpenDAMDefault"]);
        assert_eq!(lamp.source.slots, ["DefaultMaterial"]);

        // A kept slot already named OpenDAMDefault makes the added one OpenDAMDefault2
        let shelf = run("SM_Shelf").await;
        assert_eq!(shelf.added_material.as_deref(), Some("OpenDAMDefault2"));
        assert_eq!(shelf.slots, ["OpenDAMDefault2", "OpenDAMDefault"]);
        assert_eq!(shelf.source.slots, ["OpenDAMDefault"]);

        // The script refuses a name a material already holds
        let taken = Job {
            added_material: "OpenDAMDefault".into(),
            ..fixture_job("SM_Shelf", out.path())
        };
        assert_eq!(
            runner.run(&taken).await,
            Err(
                "Dungeon Props/Models/SM_Shelf.fbx: the material to add, OpenDAMDefault, is \
                 already one of its materials"
                    .into()
            )
        );

        // A material named Material is the model's own, and no default cube joins it
        let stool = run("SM_Stool").await;
        assert_eq!(stool.meshes, ["SM_Stool"]);
        assert_eq!(stool.slots, ["Material"]);
        assert_eq!(stool.added_material, None);
        assert_size(stool.bounds, [0.4, 0.5, 0.4]);
    }

    #[tokio::test]
    async fn a_take_that_moves_the_root_object_is_refused() {
        let Some(runner) = fixture_runner().await else {
            return;
        };
        let out = tempfile::tempdir().unwrap();

        // Its take Spin turns SM_Spinner itself, where the normalization goes
        let spinner = runner.run(&fixture_job("SM_Spinner", out.path())).await;

        assert_eq!(
            spinner,
            Err(
                "Dungeon Props/Models/SM_Spinner.fbx: its take Spin moves its root object \
                 SM_Spinner"
                    .into()
            )
        );
    }

    #[tokio::test]
    async fn a_take_on_several_objects_or_on_no_object_is_refused() {
        let Some(runner) = fixture_runner().await else {
            return;
        };
        let out = tempfile::tempdir().unwrap();

        // Each door's action would be written as an Open of its own
        let cabinet = runner.run(&fixture_job("SM_Cabinet", out.path())).await;
        assert_eq!(
            cabinet,
            Err(
                "Dungeon Props/Models/SM_Cabinet.fbx: its take Open animates more than one \
                 object: SM_Cabinet_DoorL, SM_Cabinet_DoorR"
                    .into()
            )
        );

        // A shape key's take would be lost
        let blob = runner.run(&fixture_job("SM_Blob", out.path())).await;
        assert_eq!(
            blob,
            Err(
                "Dungeon Props/Models/SM_Blob.fbx: its animation Key|Smile animates shape \
                 keys, not an object"
                    .into()
            )
        );
    }

    #[tokio::test]
    async fn a_take_blender_imports_under_another_name_is_refused() {
        let Some(runner) = fixture_runner().await else {
            return;
        };
        let out = tempfile::tempdir().unwrap();

        // Its two takes share a name, so Blender names the second one's action `...|SwingA.001`
        let lantern = runner.run(&fixture_job("SM_Lantern", out.path())).await;

        assert_eq!(
            lantern,
            Err("Dungeon Props/Models/SM_Lantern.fbx: its animation \
                 SM_Lantern_Glass|SM_Lantern_Glass|SwingA.001 can't be matched to an object and \
                 a take"
                .into())
        );
    }

    #[tokio::test]
    async fn blender_reads_each_fixture_model_as_the_worker_does() {
        let Some(runner) = fixture_runner().await else {
            return;
        };
        let out = tempfile::tempdir().unwrap();

        for name in [
            "SM_Totem",
            "SM_Pillar",
            "SM_Gate",
            "SM_Boulder",
            "SM_Lamp",
            "SM_Shelf",
            "SM_Stool",
            "SK_Guard",
        ] {
            let normalized = runner.run(&fixture_job(name, out.path())).await.unwrap();
            assert_eq!(
                check_facts(&normalized, &fixture_facts(name)),
                Ok(()),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn takes_keep_their_source_names() {
        let Some(runner) = fixture_runner().await else {
            return;
        };
        let out = tempfile::tempdir().unwrap();

        let guard = runner
            .run(&fixture_job("SK_Guard", out.path()))
            .await
            .unwrap();

        assert_eq!(guard.takes, ["Wave"]);
        // Read from the source file's node tree, as the preview worker reads them
        assert_eq!(guard.source.takes, ["Wave"]);
        assert_eq!(guard.source.takes, fixture_facts("SK_Guard").takes);
        assert!(guard.has_armature);
        assert_eq!(guard.bones, ["Hips", "Spine", "Head", "Head_end"]);
        assert_eq!(guard.slots, ["Cloth"]);
        assert_size(guard.bounds, [0.5, 1.8, 0.3]);
    }
}
