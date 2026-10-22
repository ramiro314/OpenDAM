//! Which Blender a game export would run, and whether it is accepted (plan C2, D29). Only an
//! export with an uncached model or animation job calls `gate`: a texture-only or fully cached
//! export never looks for Blender.

use serde::Serialize;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

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
    let path = program(configured, std::env::var_os("PATH").as_deref());
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

#[cfg(test)]
mod tests {
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
}
