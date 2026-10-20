//! Support for the dev-only scroll benchmark (`src/dev/scrollBench.ts`). This module is compiled
//! only under `debug_assertions`, so none of it reaches release builds.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::db::Database;

type Result<T> = std::result::Result<T, String>;

/// `comm` is truncated to 15 characters, so `WebKitWebProcess` reads `WebKitWebProces`.
const WEB_PROCESS_COMM: &str = "WebKitWebProces";

/// Registers `root` (from `OPENDAM_BENCH_LIBRARY`) as library `bench` in the master DB once the
/// app has created its schema, so the run script doesn't copy that schema, and opens it.
/// `Database::new` has already opened the libraries registered before it ran, so on a fresh
/// profile this opens it the way `Database::new` would have. Leaves a registered `bench` row alone.
pub async fn register_bench_library(db: &Database, root: Option<String>) {
    let Some(root) = root.filter(|r| !r.is_empty()) else {
        return;
    };
    let root_paths = serde_json::json!([root]).to_string();
    let result = sqlx::query(
        "INSERT OR IGNORE INTO libraries (id, name, root_paths, created_at)
         VALUES ('bench', 'Bench', ?, 0)",
    )
    .bind(root_paths)
    .execute(&db.master_pool)
    .await;
    if let Err(e) = result {
        tracing::error!("Could not register the bench library {root}: {e}");
        return;
    }
    if db.get_library_handle("bench").await.is_none() {
        if let Err(e) = db
            .register_library("bench", "Bench", Path::new(&root))
            .await
        {
            tracing::error!("Could not open the bench library {root}: {e}");
        }
    }
}

/// Clears the thumbnails of asset `id`'s format group in the bench library, its members' too,
/// and marks them unrendered, as a library first indexed after its renders were tied has them.
/// The files stay on disk. U11 uses it to show a model with a small render and no thumbnail.
#[tauri::command]
pub async fn bench_clear_thumbnails(
    id: String,
    state: tauri::State<'_, std::sync::Arc<crate::AppState>>,
) -> Result<u64> {
    let lib = state
        .db
        .get_library_handle("bench")
        .await
        .ok_or("no bench library")?;
    let done = sqlx::query(
        "UPDATE assets SET thumbnail_path = NULL, preview_status = 'none'
         WHERE id = ?1 OR group_id = (SELECT group_id FROM assets WHERE id = ?1)",
    )
    .bind(&id)
    .execute(&lib.pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(done.rows_affected())
}

/// Resident memory, in bytes, of the WebKit web processes this app started. Async, so the
/// `/proc` scan runs off the main thread while the harness times frames.
#[tauri::command]
pub async fn bench_rss() -> Result<u64> {
    tauri::async_runtime::spawn_blocking(web_process_rss)
        .await
        .map_err(|e| e.to_string())?
}

fn web_process_rss() -> Result<u64> {
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
/// `md/endToEnd`), or in checks mode the UI checks it runs, as comma-separated ids (`U8`), from
/// `OPENDAM_SCROLL_BENCH_ONLY`. The run script starts a fresh process per timing scenario, so no
/// scenario's RSS includes memory an earlier one left behind. Unset runs them all.
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
pub async fn bench_report(name: String, json: String) -> Result<String> {
    tauri::async_runtime::spawn_blocking(move || write_report(name, json))
        .await
        .map_err(|e| e.to_string())?
}

fn write_report(name: String, json: String) -> Result<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registers_and_opens_the_bench_library_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::new(dir.path().join("opendam.sqlite").to_str().unwrap())
            .await
            .unwrap();

        let library = tempfile::tempdir().unwrap();
        let root = library.path().to_str().unwrap().to_string();
        register_bench_library(&db, Some(root.clone())).await;
        register_bench_library(&db, Some("/elsewhere".into())).await;
        register_bench_library(&db, None).await;

        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT id, name, root_paths FROM libraries")
                .fetch_all(&db.master_pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![(
                "bench".to_string(),
                "Bench".to_string(),
                serde_json::json!([root]).to_string()
            )]
        );
        // Open in this process too, not only from the next launch
        let handle = db.get_library_handle("bench").await.expect("bench is open");
        assert_eq!(handle.root_path, library.path());
        assert!(library.path().join(".opendam/library.sqlite").exists());
    }
}
