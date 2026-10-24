//! Memory-capped child processes (plan pack-import, Runs, Capped processes): a heavy tool such as
//! Blender runs in a systemd user scope of its own with `MemoryMax`, as `scripts/memory-cap.sh`
//! runs one, so a runaway import is killed alone rather than taking the app with it.
//!
//! The scope's swap is capped at 0, and `OOMPolicy=kill` stops the whole scope at a cap kill
//! where systemd accepts it for scopes (253 on). Without a reachable `systemd-run --user`, or
//! when it refuses the cap, the command runs uncapped, after a warning.

use std::ffi::OsStr;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};

/// The systemd tools a capped command runs through, found on `PATH` unless a test names others.
#[derive(Debug, Clone)]
pub struct Systemd {
    pub run: PathBuf,
    pub ctl: PathBuf,
}

impl Default for Systemd {
    fn default() -> Self {
        Systemd {
            run: "systemd-run".into(),
            ctl: "systemctl".into(),
        }
    }
}

/// A command ready to spawn, capped or not, and what its exit status means.
pub struct Capped {
    pub command: tokio::process::Command,
    /// The cap, and the scope's unit, when it runs under one.
    scope: Option<(String, String)>,
    systemd: Systemd,
}

/// `program` with `args`, to run under a `max` memory cap (systemd's syntax, `12G`).
pub async fn capped_command<S: AsRef<OsStr>>(
    program: impl AsRef<OsStr>,
    args: impl IntoIterator<Item = S>,
    max: &str,
) -> Capped {
    capped_command_with(&Systemd::default(), program, args, max).await
}

/// `capped_command`, through the given systemd tools.
pub async fn capped_command_with<S: AsRef<OsStr>>(
    systemd: &Systemd,
    program: impl AsRef<OsStr>,
    args: impl IntoIterator<Item = S>,
    max: &str,
) -> Capped {
    let program = program.as_ref();
    let shown = program.to_string_lossy();
    let props = match scope_properties(systemd, max).await {
        Ok(props) => Some(props),
        Err(why) => {
            tracing::warn!("{why}, running {shown} without a memory cap");
            None
        }
    };
    let Some(props) = props else {
        let mut command = tokio::process::Command::new(program);
        command.args(args);
        return Capped {
            command,
            scope: None,
            systemd: systemd.clone(),
        };
    };
    let unit = format!(
        "opendam-capped-{}-{}.scope",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    );
    let mut command = tokio::process::Command::new(&systemd.run);
    command
        .args(["--user", "--scope", "--quiet"])
        .arg(format!("--unit={unit}"))
        .args(props.iter().flat_map(|p| ["-p", p.as_str()]))
        .arg("--")
        // With --scope, systemd-run execs the command, so the child is the command itself
        .arg(program)
        .args(args);
    Capped {
        command,
        scope: Some((max.to_string(), unit)),
        systemd: systemd.clone(),
    }
}

/// The scope properties systemd accepts for `max`: with `OOMPolicy=kill` where it takes it for
/// scopes, else without, tried on an empty scope first, as an older user manager refuses the
/// whole unit for one property it doesn't know.
async fn scope_properties(systemd: &Systemd, max: &str) -> Result<Vec<String>, String> {
    let unavailable = "systemd-run --user is unavailable";
    let reachable = tokio::process::Command::new(&systemd.ctl)
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());
    if !reachable {
        return Err(unavailable.into());
    }
    let capped = [format!("MemoryMax={max}"), "MemorySwapMax=0".to_string()];
    let with_policy: Vec<String> = capped
        .iter()
        .cloned()
        .chain(["OOMPolicy=kill".to_string()])
        .collect();
    for props in [with_policy, capped.to_vec()] {
        let accepted = tokio::process::Command::new(&systemd.run)
            .args(["--user", "--scope", "--quiet"])
            .args(props.iter().flat_map(|p| ["-p", p.as_str()]))
            .args(["--", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        match accepted {
            Ok(status) if status.success() => return Ok(props),
            Ok(_) => {}
            Err(_) => return Err(unavailable.into()),
        }
    }
    Err(format!("systemd-run --user refused MemoryMax={max}"))
}

impl Capped {
    /// The cap it runs under, `None` when uncapped.
    pub fn cap(&self) -> Option<&str> {
        self.scope.as_ref().map(|(max, _)| max.as_str())
    }

    /// Why a run that exited with `status` was stopped by the cap: status 137 or SIGKILL under a
    /// cap. `None` for any other status, and for every status of an uncapped run.
    pub fn killed_by_cap(&self, status: ExitStatus) -> Option<String> {
        let (max, _) = self.scope.as_ref()?;
        (status.code() == Some(137) || status.signal() == Some(9))
            .then(|| format!("killed by the memory cap ({max})"))
    }

    /// Clears the scope's failed state, which systemd keeps until reset, after a run that
    /// didn't exit cleanly.
    pub async fn finish(&self, status: ExitStatus) {
        if let (Some((_, unit)), false) = (&self.scope, status.success()) {
            let _ = tokio::process::Command::new(&self.systemd.ctl)
                .args(["--user", "reset-failed", unit])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
    }
}

/// Whether `max` is a cap systemd's `MemoryMax=` reads: bytes, or a number with a `K`, `M`, `G`
/// or `T` suffix.
pub fn check_cap(max: &str) -> Result<(), String> {
    let digits = max.strip_suffix(['K', 'M', 'G', 'T']).unwrap_or(max);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && digits != "0" {
        Ok(())
    } else {
        Err(format!(
            "{max:?} isn't a memory cap: give a size such as 12G (K, M, G or T)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A systemd that accepts every scope and runs its command, logging each call.
    fn stub_systemd(dir: &Path) -> Systemd {
        let log = dir.join("calls.log");
        Systemd {
            run: script(
                dir,
                "systemd-run",
                &format!(
                    "echo \"run $*\" >> '{}'\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"",
                    log.display()
                ),
            ),
            ctl: script(dir, "systemctl", &format!("echo \"ctl $*\" >> '{}'", log.display())),
        }
    }

    async fn status(capped: &mut Capped) -> ExitStatus {
        capped.command.stdin(Stdio::null()).status().await.unwrap()
    }

    #[tokio::test]
    async fn a_process_exiting_137_under_the_cap_was_killed_by_it() {
        let dir = tempfile::tempdir().unwrap();
        let systemd = stub_systemd(dir.path());
        let mut capped = capped_command_with(&systemd, "sh", ["-c", "exit 137"], "12G").await;
        assert_eq!(capped.cap(), Some("12G"));
        let exited = status(&mut capped).await;
        assert_eq!(exited.code(), Some(137));
        assert_eq!(
            capped.killed_by_cap(exited).as_deref(),
            Some("killed by the memory cap (12G)")
        );
        capped.finish(exited).await;

        let calls = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        let run = calls
            .lines()
            .find(|l| l.starts_with("run ") && l.contains("-- sh"))
            .unwrap();
        for prop in [
            "MemoryMax=12G",
            "MemorySwapMax=0",
            "OOMPolicy=kill",
            "--scope",
        ] {
            assert!(run.contains(prop), "{run}");
        }
        assert!(
            calls.contains("ctl --user reset-failed opendam-capped-"),
            "{calls}"
        );

        // Any other failure is the command's own.
        let mut capped = capped_command_with(&systemd, "sh", ["-c", "exit 3"], "12G").await;
        let exited = status(&mut capped).await;
        assert_eq!(capped.killed_by_cap(exited), None);
    }

    #[tokio::test]
    async fn without_systemd_run_user_it_runs_uncapped() {
        let dir = tempfile::tempdir().unwrap();
        // No user manager to reach, as in a CI container.
        let systemd = Systemd {
            run: script(dir.path(), "systemd-run", "exit 1"),
            ctl: script(dir.path(), "systemctl", "exit 1"),
        };
        let mut capped = capped_command_with(&systemd, "sh", ["-c", "exit 137"], "12G").await;
        assert_eq!(capped.cap(), None);
        let exited = status(&mut capped).await;
        assert_eq!(exited.code(), Some(137));
        assert_eq!(
            capped.killed_by_cap(exited),
            None,
            "an uncapped run has no cap"
        );

        // Nor without the tools at all.
        let missing = Systemd {
            run: dir.path().join("none-run"),
            ctl: dir.path().join("none-ctl"),
        };
        let mut capped = capped_command_with(&missing, "true", [] as [&str; 0], "12G").await;
        assert_eq!(capped.cap(), None);
        assert!(status(&mut capped).await.success());
    }

    #[tokio::test]
    async fn an_older_systemd_without_oom_policy_still_caps() {
        let dir = tempfile::tempdir().unwrap();
        let mut systemd = stub_systemd(dir.path());
        systemd.run = script(
            dir.path(),
            "systemd-run",
            "case \"$*\" in *OOMPolicy*) exit 1;; esac\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nexec \"$@\"",
        );
        let mut capped = capped_command_with(&systemd, "true", [] as [&str; 0], "1G").await;
        assert_eq!(capped.cap(), Some("1G"));
        assert!(status(&mut capped).await.success());
    }

    /// Under a real user manager, a process past its cap is killed and reported. Skips where
    /// `systemd-run --user` is unavailable, as in CI.
    #[tokio::test]
    async fn a_real_cap_kills_a_process_that_outgrows_it() {
        let systemd = Systemd::default();
        if scope_properties(&systemd, "64M").await.is_err() {
            eprintln!("Skipping: systemd-run --user is unavailable");
            return;
        }
        // 256 MiB of touched memory, four times the cap.
        let grow = "a = bytearray(256 * 1024 * 1024)\nfor i in range(0, len(a), 4096): a[i] = 1";
        let mut capped = capped_command_with(&systemd, "python3", ["-c", grow], "64M").await;
        let exited = status(&mut capped).await;
        capped.finish(exited).await;
        assert_eq!(
            capped.killed_by_cap(exited).as_deref(),
            Some("killed by the memory cap (64M)"),
            "{exited}"
        );
    }

    #[test]
    fn caps_are_sizes_systemd_reads() {
        for ok in ["12G", "512M", "1073741824", "2T", "64K"] {
            assert_eq!(check_cap(ok), Ok(()), "{ok}");
        }
        for bad in [
            "", "G", "12g", "12 G", "-1G", "1.5G", "0", "12GB", "infinity",
        ] {
            assert!(check_cap(bad).is_err(), "{bad}");
        }
    }
}
