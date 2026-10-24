//! What Settings' Import tools section shows besides Blender, which the export's status covers:
//! the `assimp` program, the Unreal Editor, and the conversion cache's size (plan pack-import,
//! Settings). Each check reads the values typed in Settings, saved or not.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;

/// The program `unity_glb` runs for an ASCII or old FBX when `import.assimpPath` is unset,
/// found on `PATH`.
pub const ASSIMP: &str = "assimp";

/// How long `assimp version` has to answer.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// A tool as Settings shows it: where it is, its version, and why a step can't run it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStatus {
    pub path: Option<String>,
    pub version: Option<String>,
    pub problem: Option<String>,
}

/// The tools Settings checks for the import, from the values it holds now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTools {
    pub assimp: ToolStatus,
    pub unreal_editor: ToolStatus,
    /// Whether UnityPy imports in the preview engine, `None` until its worker has said.
    pub unitypy: Option<bool>,
}

/// The conversion cache as Settings shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheStatus {
    pub path: String,
    /// The bytes of every file below it.
    pub bytes: u64,
}

/// `assimp` at `configured` (an absolute path), else on `PATH`, and its version.
pub async fn assimp_status(configured: Option<&str>) -> ToolStatus {
    assimp_status_on(configured, std::env::var_os("PATH").as_deref()).await
}

/// `assimp_status`, looking for `assimp` on `path_var` when no path is configured.
async fn assimp_status_on(
    configured: Option<&str>,
    path_var: Option<&std::ffi::OsStr>,
) -> ToolStatus {
    if let Some(path) = configured {
        if let Err(problem) = super::settings::check("assimpPath", path) {
            return ToolStatus {
                problem: Some(problem),
                ..Default::default()
            };
        }
    }
    let Some(program) = program(configured, ASSIMP, path_var) else {
        return ToolStatus {
            problem: Some(match configured {
                Some(path) => format!("No assimp at {path}"),
                None => format!("{ASSIMP} isn't on PATH: install assimp or set its path"),
            }),
            ..Default::default()
        };
    };
    let mut status = ToolStatus {
        path: Some(program.display().to_string()),
        ..Default::default()
    };
    let output = tokio::time::timeout(
        VERSION_TIMEOUT,
        tokio::process::Command::new(&program)
            .arg("version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match output {
        Ok(Ok(output)) => {
            status.version = assimp_version(&String::from_utf8_lossy(&output.stdout));
            if status.version.is_none() {
                status.problem = Some(format!("{} didn't report a version", program.display()));
            }
        }
        Ok(Err(e)) => status.problem = Some(format!("Could not start {}: {e}", program.display())),
        Err(_) => {
            status.problem = Some(format!(
                "{} version didn't answer in {} s",
                program.display(),
                VERSION_TIMEOUT.as_secs()
            ))
        }
    }
    status
}

/// The version in `assimp version`'s output, "6.0" from "Version 6.0 -shared -st (GIT commit 0)".
fn assimp_version(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let version = line
            .trim()
            .strip_prefix("Version ")?
            .split_whitespace()
            .next()?;
        version
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| version.to_string())
    })
}

/// The Unreal Editor below `root` and its version, as `export_unreal` would find it.
pub fn unreal_status(root: &Path) -> ToolStatus {
    let root = root.to_string_lossy();
    if let Err(problem) = super::settings::check("unrealEditorRoot", &root) {
        return ToolStatus {
            problem: Some(problem),
            ..Default::default()
        };
    }
    match super::unreal_glb::editor(Path::new(root.as_ref())) {
        Ok(editor) => ToolStatus {
            path: Some(editor.program.display().to_string()),
            version: Some(editor.version),
            problem: None,
        },
        Err(problem) => ToolStatus {
            problem: Some(problem),
            ..Default::default()
        },
    }
}

/// The executable at `configured`, else `name` on `path_var`.
fn program(
    configured: Option<&str>,
    name: &str,
    path_var: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    match configured {
        Some(path) => Some(PathBuf::from(path)).filter(|p| is_executable(p)),
        None => std::env::split_paths(path_var?)
            .map(|dir| dir.join(name))
            .find(|p| is_executable(p)),
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The bytes of every file below `dir`, following no symlink, 0 when it doesn't exist.
pub fn cache_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(m) if m.is_dir() => cache_size(&entry.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Deletes everything below `dir`, keeping the folder. One it can't delete fails the clear,
/// naming it, after the rest are deleted.
pub fn clear_cache(dir: &Path) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("Could not read {}: {e}", dir.display())),
    };
    let mut failed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let removed = match entry.file_type() {
            Ok(t) if t.is_dir() => std::fs::remove_dir_all(&path),
            _ => std::fs::remove_file(&path),
        };
        if let Err(e) = removed {
            failed.push(format!("{} ({e})", path.display()));
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("Could not delete {}", failed.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn assimp_s_version_is_read_from_its_banner() {
        let banner =
            "------\nOpen Asset Import Library (\"Assimp\")\n -- Commandline toolchain --\n\n\
                      Version 6.0 -shared -st (GIT commit 0)\n";
        assert_eq!(assimp_version(banner).as_deref(), Some("6.0"));
        assert_eq!(assimp_version("Version unknown\n"), None);
        assert_eq!(assimp_version(""), None);
    }

    #[tokio::test]
    async fn assimp_is_found_at_its_path_or_reported_missing() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("assimp");
        std::fs::write(&fake, "#!/bin/sh\necho 'Version 5.4.3 -shared'\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let found = assimp_status(Some(fake.to_str().unwrap())).await;
        assert_eq!(
            found,
            ToolStatus {
                path: Some(fake.display().to_string()),
                version: Some("5.4.3".into()),
                problem: None,
            }
        );

        let gone = dir.path().join("gone");
        let missing = assimp_status(Some(gone.to_str().unwrap())).await;
        assert_eq!(missing.path, None);
        assert_eq!(
            missing.problem,
            Some(format!("No assimp at {}", gone.display()))
        );

        let relative = assimp_status(Some("bin/assimp")).await;
        assert!(relative.problem.unwrap().contains("absolute path"));
    }

    /// A fake `assimp` in `dir` whose `version` prints `output`.
    fn fake_assimp(dir: &Path, output: &str) -> PathBuf {
        let fake = dir.join("assimp");
        std::fs::write(&fake, format!("#!/bin/sh\necho '{output}'\n")).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        fake
    }

    #[tokio::test]
    async fn unset_assimp_is_looked_up_on_path() {
        let empty = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let fake = fake_assimp(bin.path(), "Version 6.0 -shared -st (GIT commit 0)");

        let path_var = std::env::join_paths([empty.path(), bin.path()]).unwrap();
        assert_eq!(
            assimp_status_on(None, Some(&path_var)).await,
            ToolStatus {
                path: Some(fake.display().to_string()),
                version: Some("6.0".into()),
                problem: None,
            }
        );

        let without = std::env::join_paths([empty.path()]).unwrap();
        for path_var in [Some(without.as_os_str()), None] {
            assert_eq!(
                assimp_status_on(None, path_var).await,
                ToolStatus {
                    problem: Some("assimp isn't on PATH: install assimp or set its path".into()),
                    ..Default::default()
                }
            );
        }
    }

    #[tokio::test]
    async fn an_assimp_that_reports_no_version_is_found_but_refused() {
        let dir = tempfile::tempdir().unwrap();
        let fake = fake_assimp(dir.path(), "Open Asset Import Library");
        let status = assimp_status(Some(fake.to_str().unwrap())).await;
        assert_eq!(
            status,
            ToolStatus {
                path: Some(fake.display().to_string()),
                version: None,
                problem: Some(format!("{} didn't report a version", fake.display())),
            }
        );
    }

    #[test]
    fn the_unreal_editor_is_checked_below_its_root() {
        let dir = tempfile::tempdir().unwrap();
        let missing = unreal_status(dir.path());
        assert!(
            missing.problem.as_deref().unwrap().contains("no editor at"),
            "{missing:?}"
        );

        let program = dir.path().join("Engine/Binaries/Linux/UnrealEditor");
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::write(&program, "").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(dir.path().join("Engine/Build")).unwrap();
        std::fs::write(
            dir.path().join("Engine/Build/Build.version"),
            r#"{"MajorVersion": 5, "MinorVersion": 6, "PatchVersion": 1}"#,
        )
        .unwrap();
        assert_eq!(
            unreal_status(dir.path()),
            ToolStatus {
                path: Some(program.display().to_string()),
                version: Some("5.6.1".into()),
                problem: None,
            }
        );

        let relative = unreal_status(Path::new("Applications/UnrealEngine"));
        assert!(relative.problem.unwrap().contains("absolute folder"));
    }

    #[test]
    fn clearing_the_cache_empties_it_and_keeps_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("import");
        assert_eq!(cache_size(&cache), 0);
        clear_cache(&cache).unwrap();

        std::fs::create_dir_all(cache.join("glb")).unwrap();
        std::fs::create_dir_all(cache.join("work/run/deep")).unwrap();
        std::fs::write(cache.join("glb/a.glb"), [0u8; 100]).unwrap();
        std::fs::write(cache.join("work/run/deep/b.fbx"), [0u8; 23]).unwrap();
        std::fs::write(cache.join("loose.json"), "{}").unwrap();
        // A symlink counts as itself and is deleted, never what it points at.
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep.bin"), [0u8; 1000]).unwrap();
        std::os::unix::fs::symlink(&outside, cache.join("link")).unwrap();
        let link_len = std::fs::symlink_metadata(cache.join("link")).unwrap().len();
        assert_eq!(cache_size(&cache), 125 + link_len);

        clear_cache(&cache).unwrap();
        assert!(cache.is_dir());
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
        assert_eq!(cache_size(&cache), 0);
        assert!(outside.join("keep.bin").exists());
    }

    #[test]
    fn an_entry_clear_can_t_delete_fails_it_by_name_after_the_rest_go() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("import");
        let stuck = cache.join("work");
        std::fs::create_dir_all(&stuck).unwrap();
        std::fs::write(stuck.join("held.fbx"), [0u8; 10]).unwrap();
        std::fs::create_dir_all(cache.join("glb")).unwrap();
        std::fs::write(cache.join("glb/a.glb"), [0u8; 100]).unwrap();
        std::fs::write(cache.join("loose.json"), "{}").unwrap();
        // A folder whose files can't be unlinked
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o555)).unwrap();

        let failed = clear_cache(&cache);
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = failed.unwrap_err();
        assert!(
            error.starts_with(&format!("Could not delete {} (", stuck.display())),
            "{error}"
        );
        assert!(cache.is_dir());
        let mut left: Vec<_> = std::fs::read_dir(&cache)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(left, ["work"], "the rest are deleted");
        assert!(stuck.join("held.fbx").exists());
        assert_eq!(cache_size(&cache), 10);
    }
}
