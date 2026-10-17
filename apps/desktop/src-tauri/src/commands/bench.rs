//! Support for the dev-only scroll benchmark (`src/dev/scrollBench.ts`). This module is compiled
//! only under `debug_assertions`, so none of it reaches release builds.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, String>;

/// `comm` is truncated to 15 characters, so `WebKitWebProcess` reads `WebKitWebProces`.
const WEB_PROCESS_COMM: &str = "WebKitWebProces";

/// Resident memory, in bytes, of the WebKit web processes this app started.
#[tauri::command]
pub fn bench_rss() -> Result<u64> {
    let mut parent_of = HashMap::new();
    let mut comm_of = HashMap::new();
    for entry in std::fs::read_dir("/proc").map_err(|e| e.to_string())? {
        let Some(pid) = entry
            .ok()
            .and_then(|e| e.file_name().to_str()?.parse::<u32>().ok())
        else {
            continue;
        };
        // `stat` is `pid (comm) state ppid ...`, and comm may hold spaces or parentheses.
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let ppid = stat[close + 1..]
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse::<u32>().ok());
        if let Some(ppid) = ppid {
            parent_of.insert(pid, ppid);
            comm_of.insert(pid, stat[open + 1..close].to_string());
        }
    }

    let me = std::process::id();
    let descends_from_me = |mut pid: u32| {
        while let Some(&ppid) = parent_of.get(&pid) {
            if ppid == me {
                return true;
            }
            pid = ppid;
        }
        false
    };

    let mut total_kb = 0;
    for (&pid, comm) in &comm_of {
        if !comm.starts_with(WEB_PROCESS_COMM) || !descends_from_me(pid) {
            continue;
        }
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        total_kb += status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0);
    }
    Ok(total_kb * 1024)
}

/// The rendering mode the app runs in (`render::describe_render_env`).
#[tauri::command]
pub fn bench_render_mode() -> &'static str {
    crate::render::current_render_mode()
}

/// The one timing scenario this app process runs, as `<size>/<scenario>` (for example
/// `md/endToEnd`), from `OPENDAM_SCROLL_BENCH_ONLY`. The run script starts a fresh process per
/// scenario, so no scenario's RSS includes memory an earlier one left behind. Unset runs them all.
#[tauri::command]
pub fn bench_only() -> Option<String> {
    std::env::var("OPENDAM_SCROLL_BENCH_ONLY")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Prints one harness line to the app's stdout, where the nested-KWin script watches for
/// progress, screenshot pauses and the end of a run. The webview's console doesn't reach it.
#[tauri::command]
pub fn bench_log(line: String) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "[scroll-bench] {line}");
    let _ = out.flush();
}

/// Adds the git commit to the report's `context`, prints the report and writes it to
/// `src-tauri/target/scroll-bench/<name>-<unix-ts>.json`. Returns the file's path.
#[tauri::command]
pub fn bench_report(name: String, json: String) -> Result<String> {
    let mut report: serde_json::Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(context) = report.get_mut("context").and_then(|c| c.as_object_mut()) {
        context.insert(
            "gitCommit".into(),
            git(manifest_dir, &["rev-parse", "HEAD"]).into(),
        );
        let dirty = git(manifest_dir, &["status", "--porcelain"]).map(|s| !s.is_empty());
        context.insert("gitDirty".into(), dirty.into());
    }

    let name: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let dir: PathBuf = manifest_dir.join("target").join("scroll-bench");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{name}-{ts}.json"));
    let pretty = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    std::fs::write(&path, &pretty).map_err(|e| e.to_string())?;

    let path = path.display().to_string();
    bench_log(format!("report {path}\n{pretty}"));
    Ok(path)
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
