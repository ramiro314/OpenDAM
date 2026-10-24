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

/// How much of a child's stdout, and of its stderr, `Capped::watch` keeps: their ends, which
/// say why a run failed. An editor session can log hundreds of MB, outside the child's cap.
const KEPT: usize = 64 * 1024;

/// Appends `bytes` to `buffer`, keeping its last `KEPT` bytes at least. It grows to twice
/// that before its start is dropped, so a long log isn't copied at every line.
fn keep_tail(buffer: &mut Vec<u8>, bytes: &[u8]) {
    buffer.extend_from_slice(bytes);
    if buffer.len() > 2 * KEPT {
        buffer.drain(..buffer.len() - KEPT);
    }
}

/// How long `Capped::watch` goes on reading a child's output once it has exited.
const DRAIN: std::time::Duration = std::time::Duration::from_secs(2);

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

    /// Spawns the command and waits for it, its stdin closed and its output piped: `line` sees
    /// each line it prints to stdout as it prints it, and `cancelled` is polled every 200 ms.
    /// When that turns true the child is killed and the result is `None`. The end of stdout and
    /// of stderr (`KEPT`) come back in the output, and the scope is `finish`ed. However it ends,
    /// whatever the child left running is killed (`stop_leftovers`).
    pub async fn watch(
        &mut self,
        mut line: impl FnMut(&[u8]),
        cancelled: impl Fn() -> bool,
    ) -> std::io::Result<Option<std::process::Output>> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        let mut child = self
            .command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            // A group of its own, so its descendants can be killed with it
            .process_group(0)
            .spawn()?;
        let pid = child.id();
        let mut stderr = child.stderr.take().expect("piped");
        // Kept as it arrives, so what came before a drain that timed out isn't lost.
        let errors = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut reader = tokio::spawn({
            let errors = errors.clone();
            async move {
                let mut chunk = [0u8; 8192];
                while let Ok(n @ 1..) = stderr.read(&mut chunk).await {
                    let mut errors = errors.lock().unwrap_or_else(|e| e.into_inner());
                    keep_tail(&mut errors, &chunk[..n]);
                }
            }
        });
        // Read as bytes: a line that isn't UTF-8 must not stop the reading, or a full pipe
        // would block the child.
        let mut lines = tokio::io::BufReader::new(child.stdout.take().expect("piped")).split(b'\n');
        let mut printed = Vec::new();
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
        let mut reading = true;
        let status = loop {
            tokio::select! {
                read = lines.next_segment(), if reading => match read {
                    Ok(Some(read)) => {
                        line(&read);
                        keep_tail(&mut printed, &read);
                        keep_tail(&mut printed, b"\n");
                    }
                    _ => reading = false,
                },
                status = child.wait() => break status?,
                _ = tick.tick() => {
                    if cancelled() {
                        let _ = child.start_kill();
                        let killed = child.wait().await;
                        self.stop_leftovers(pid).await;
                        // A scope killed by a signal stays failed until it is reset.
                        if let Ok(status) = killed {
                            self.finish(status).await;
                        }
                        return Ok(None);
                    }
                }
            }
        };
        self.stop_leftovers(pid).await;
        // What the child printed last is read to the pipes' end, but only for `DRAIN`: a
        // descendant that inherited them (the Unreal Editor's shader workers) can hold them open
        // long after the child exited, and the run must end with the child.
        let drained = async {
            while let Ok(Some(read)) = lines.next_segment().await {
                keep_tail(&mut printed, &read);
                keep_tail(&mut printed, b"\n");
            }
            let _ = (&mut reader).await;
        };
        if tokio::time::timeout(DRAIN, drained).await.is_err() {
            tracing::warn!("A process kept the output of a finished child open; reading stopped");
        }
        reader.abort();
        self.finish(status).await;
        let stderr = std::mem::take(&mut *errors.lock().unwrap_or_else(|e| e.into_inner()));
        Ok(Some(std::process::Output {
            status,
            stdout: printed,
            stderr,
        }))
    }

    /// Kills what the child with pid `pid` left running: its process group, and under a cap
    /// everything left in its scope, which also holds the descendants that left the group. A
    /// helper that outlived the child (the Unreal Editor's shader workers) would otherwise run
    /// on outside the next session's cap, or on a work folder being deleted. Nothing left to
    /// kill is no error.
    async fn stop_leftovers(&self, pid: Option<u32>) {
        let quiet = |command: &mut tokio::process::Command| {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        };
        if let Some(pid) = pid {
            let group = format!("-{pid}");
            let _ = quiet(tokio::process::Command::new("kill").args(["-KILL", "--", &group])).await;
        }
        if let Some((_, unit)) = &self.scope {
            let mut kill = tokio::process::Command::new(&self.systemd.ctl);
            kill.args(["--user", "kill", "--signal=SIGKILL", unit]);
            let _ = quiet(&mut kill).await;
        }
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
    let digits_only = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    // A cap of zero, however many zeros it is written with, would kill every run at once.
    if digits_only && digits.bytes().any(|b| b != b'0') {
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

    /// What `tracing` logs while the guard lives, on this thread (a `tokio::test` runs on one).
    #[derive(Clone, Default)]
    struct Logged(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Logged {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logged {
        type Writer = Logged;

        fn make_writer(&'a self) -> Logged {
            self.clone()
        }
    }

    impl Logged {
        fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
            let logged = Logged::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(logged.clone())
                .with_ansi(false)
                .finish();
            (logged, tracing::subscriber::set_default(subscriber))
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
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
    async fn without_systemd_run_user_it_warns_and_runs_uncapped() {
        let (logged, _guard) = Logged::capture();
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
        let warned = logged.text();
        assert!(
            warned.contains("WARN")
                && warned
                    .contains("systemd-run --user is unavailable, running sh without a memory cap"),
            "{warned}"
        );

        // Nor without the tools at all.
        let missing = Systemd {
            run: dir.path().join("none-run"),
            ctl: dir.path().join("none-ctl"),
        };
        let mut capped = capped_command_with(&missing, "true", [] as [&str; 0], "12G").await;
        assert_eq!(capped.cap(), None);
        assert!(status(&mut capped).await.success());
        assert!(
            logged
                .text()
                .contains("systemd-run --user is unavailable, running true without a memory cap"),
            "{}",
            logged.text()
        );
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

    /// A descendant that keeps the pipes open past the child's exit, as an editor's helper can,
    /// doesn't hold the run: the output is read for `DRAIN` at most, keeping what came.
    #[tokio::test]
    async fn watch_ends_with_the_child_whatever_keeps_its_pipes_open() {
        let dir = tempfile::tempdir().unwrap();
        let uncapped = Systemd {
            run: dir.path().join("none-run"),
            ctl: dir.path().join("none-ctl"),
        };
        // Out of the child's process group, so the group kill doesn't reach it, and uncapped,
        // with no scope to kill: only the drain's bound ends the run. The child exits only once
        // the descendant, writing `left` from its own session, has left the group.
        let left = dir.path().join("left");
        let script = format!(
            "echo out; echo err >&2; setsid sh -c 'echo $$ > {left}; exec sleep 20' & \
             while [ ! -s {left} ]; do sleep 0.05; done; exit 0",
            left = left.display()
        );
        let mut capped = capped_command_with(&uncapped, "sh", ["-c", script.as_str()], "1G").await;
        let started = std::time::Instant::now();
        let output = capped.watch(|_| {}, || false).await.unwrap().unwrap();
        let took = started.elapsed();
        assert!(took >= DRAIN && took < DRAIN * 3, "{took:?}");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }

    /// Whether process `pid` is gone, or a zombie waiting to be reaped, within 3 s.
    async fn ends(pid: u32) -> bool {
        for _ in 0..30 {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => return true,
                Ok(stat) if stat.rsplit(") ").next().is_some_and(|s| s.starts_with('Z')) => {
                    return true
                }
                Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
        false
    }

    fn pids(file: &Path) -> Vec<u32> {
        std::fs::read_to_string(file)
            .unwrap_or_default()
            .split_whitespace()
            .map(|p| p.parse().unwrap())
            .collect()
    }

    /// What the child leaves running is killed once it exits, and with it on Cancel; under a
    /// cap, the rest of its scope is killed too.
    #[tokio::test]
    async fn watch_kills_what_the_child_leaves_running() {
        let dir = tempfile::tempdir().unwrap();
        let uncapped = Systemd {
            run: dir.path().join("none-run"),
            ctl: dir.path().join("none-ctl"),
        };
        let file = dir.path().join("pids");
        let leave = format!(
            "sleep 30 >/dev/null 2>&1 & echo $! > '{}'; exit 0",
            file.display()
        );
        let mut capped = capped_command_with(&uncapped, "sh", ["-c", leave.as_str()], "1G").await;
        let started = std::time::Instant::now();
        assert!(capped.watch(|_| {}, || false).await.unwrap().is_some());
        let [helper] = pids(&file)[..] else { panic!() };
        assert!(ends(helper).await, "the helper {helper} still runs");
        // Its pipes closed with it, so no drain waited for them.
        assert!(started.elapsed() < DRAIN, "{:?}", started.elapsed());

        // Cancelled mid-run, the child and what it started both end.
        std::fs::remove_file(&file).unwrap();
        let run = format!(
            "sleep 30 >/dev/null 2>&1 & echo $$ $! > '{}'; sleep 30",
            file.display()
        );
        let mut capped = capped_command_with(&uncapped, "sh", ["-c", run.as_str()], "1G").await;
        let cancelled = || pids(&file).len() == 2;
        assert!(capped.watch(|_| {}, cancelled).await.unwrap().is_none());
        for pid in pids(&file) {
            assert!(ends(pid).await, "{pid} still runs");
        }

        // Under a cap, the scope is killed as well.
        let systemd = stub_systemd(dir.path());
        let mut capped = capped_command_with(&systemd, "true", [] as [&str; 0], "1G").await;
        capped.watch(|_| {}, || false).await.unwrap();
        let calls = || std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
        assert!(
            calls().contains("ctl --user kill --signal=SIGKILL opendam-capped-"),
            "{}",
            calls()
        );
        // A clean exit leaves no failed scope to reset; a cancelled run's killed scope is reset.
        assert!(!calls().contains("reset-failed"), "{}", calls());
        let mut capped = capped_command_with(&systemd, "sleep", ["30"], "1G").await;
        assert!(capped.watch(|_| {}, || true).await.unwrap().is_none());
        assert!(
            calls().contains("ctl --user reset-failed opendam-capped-"),
            "{}",
            calls()
        );
    }

    /// A long log keeps only its end, on both streams: what says why a run failed.
    #[tokio::test]
    async fn watch_keeps_the_end_of_a_long_log() {
        let dir = tempfile::tempdir().unwrap();
        let uncapped = Systemd {
            run: dir.path().join("none-run"),
            ctl: dir.path().join("none-ctl"),
        };
        // About 2 MB on each stream, then a last line.
        let script = "i=0; while [ $i -lt 40000 ]; do \
                      echo \"line $i of a long editor log, padded out\"; \
                      echo \"error line $i of a long editor log\" >&2; i=$((i+1)); done; \
                      echo last; echo last error >&2";
        let mut capped = capped_command_with(&uncapped, "sh", ["-c", script], "1G").await;
        let mut lines = 0;
        let output = capped
            .watch(|_| lines += 1, || false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lines, 40001, "every line still reaches the callback");
        for stream in [&output.stdout, &output.stderr] {
            assert!(
                (KEPT..=2 * KEPT).contains(&stream.len()),
                "{}",
                stream.len()
            );
        }
        assert!(output
            .stdout
            .ends_with(b"line 39999 of a long editor log, padded out\nlast\n"));
        assert!(output.stderr.ends_with(b"last error\n"));
    }

    #[test]
    fn caps_are_sizes_systemd_reads() {
        for ok in ["12G", "512M", "1073741824", "2T", "64K", "016G"] {
            assert_eq!(check_cap(ok), Ok(()), "{ok}");
        }
        for bad in [
            "", "G", "12g", "12 G", "-1G", "1.5G", "0", "00", "00G", "000M", "12GB", "infinity",
        ] {
            assert!(check_cap(bad).is_err(), "{bad}");
        }
    }
}
