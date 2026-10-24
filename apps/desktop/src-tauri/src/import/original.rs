//! The original's state (plan pack-import, The original's state). Each import's original is
//! checked by size and mtime at startup and when the Imports view opens, and hashed again only
//! when either changed. A missing original marks the import `missing`, and a file at its path
//! whose sha256 differs marks it `changed`, keeping that file's stamp and sha256 (`seen_*`) so it
//! isn't hashed again until it changes again. Either refuses its packs' Rebuild, while the packs
//! keep working. Locate original re-points the import at a file with the same sha256, and at a
//! different file only once it is named by its sha256 (Use this file).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use sqlx::{Pool, Sqlite};

use super::build::file_stamp;
use super::records::BUSY;
use crate::export::hashes::sha256_file;

/// What Rebuild answers while the original is missing.
pub const MISSING: &str = "The original is missing: locate it to rebuild";
/// What Rebuild answers while the file at the original's path differs from it.
pub const CHANGED: &str =
    "The original changed since it was imported: locate it, or use the changed file, to rebuild";

/// Why the original's state refuses a Rebuild, or `None` when it is `ok`.
pub fn refusal(state: &str) -> Option<&'static str> {
    match state {
        "missing" => Some(MISSING),
        "changed" => Some(CHANGED),
        _ => None,
    }
}

/// A file's size and mtime in nanoseconds, as `build::file_stamp` reads them.
pub type Stamp = (i64, i64);

/// An import's original as recorded, with the changed file the last check found at its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub id: String,
    pub stamp: Stamp,
    pub sha256: String,
    pub seen: Option<(Stamp, String)>,
}

/// What a check found at an import's original path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// The recorded file, by its stamp.
    Same,
    /// The recorded file under a new stamp (touched, or copied back), by its sha256.
    Restamped(Stamp),
    Missing,
    /// Another file, with its stamp and sha256.
    Changed(Stamp, String),
}

impl Found {
    pub fn state(&self) -> &'static str {
        match self {
            Found::Same | Found::Restamped(_) => "ok",
            Found::Missing => "missing",
            Found::Changed(..) => "changed",
        }
    }
}

/// The file at `path` against each import recorded there. Nothing is hashed when the file's
/// stamp is each import's recorded or seen one, and the file is hashed at most once.
pub fn check(path: &Path, imports: &[Recorded]) -> Vec<Found> {
    let stamp = match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => file_stamp(path).ok(),
        _ => None,
    };
    let Some((size, mtime_ns)) = stamp else {
        return vec![Found::Missing; imports.len()];
    };
    let stamp = (size as i64, mtime_ns);
    let mut hashed: Option<Option<String>> = None;
    imports
        .iter()
        .map(|import| {
            if import.stamp == stamp {
                return Found::Same;
            }
            if let Some((seen, sha256)) = &import.seen {
                if *seen == stamp {
                    return Found::Changed(stamp, sha256.clone());
                }
            }
            let sha256 = hashed.get_or_insert_with(|| sha256_file(path).ok());
            match sha256 {
                None => Found::Missing,
                Some(sha256) if *sha256 == import.sha256 => Found::Restamped(stamp),
                Some(sha256) => Found::Changed(stamp, sha256.clone()),
            }
        })
        .collect()
}

/// One lock per original path, so two checks never hash the same file at once, while a check of
/// one original never waits for another's hashing.
#[derive(Default)]
pub struct PathLocks(std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>);

impl PathLocks {
    pub async fn lock(&self, path: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.0.lock().unwrap_or_else(|e| e.into_inner());
            // Paths no check holds or waits for are dropped, so the map stays as small as the
            // checks under way
            locks.retain(|_, l| Arc::strong_count(l) > 1);
            locks.entry(path.to_string()).or_default().clone()
        };
        lock.lock_owned().await
    }
}

/// Checks every import's original in the library, or only import `only`'s, and records what
/// each check found, holding each original path's lock from reading its imports to recording
/// them. Returns whether any import's state changed.
pub async fn check_library(
    pool: &Pool<Sqlite>,
    locks: &PathLocks,
    only: Option<&str>,
) -> sqlx::Result<bool> {
    let paths: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT original_path FROM imports WHERE ?1 IS NULL OR id = ?1
         ORDER BY original_path",
    )
    .bind(only)
    .fetch_all(pool)
    .await?;
    let mut changed = false;
    for path in paths {
        let _held = locks.lock(&path).await;
        // Read under the lock, so a check that waited for another of the same file sees the
        // stamp it recorded and hashes nothing again
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            String,
            i64,
            i64,
            String,
            String,
            Option<i64>,
            Option<i64>,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT id, original_size, original_mtime_ns, original_sha256, original_state,
             seen_size, seen_mtime_ns, seen_sha256 FROM imports
             WHERE original_path = ?1 AND (?2 IS NULL OR id = ?2)",
        )
        .bind(&path)
        .bind(only)
        .fetch_all(pool)
        .await?;
        let imports: Vec<(Recorded, String)> = rows
            .into_iter()
            .map(
                |(id, size, mtime_ns, sha256, state, seen_size, seen_mtime, seen_sha)| {
                    let seen = match (seen_size, seen_mtime, seen_sha) {
                        (Some(size), Some(mtime), Some(sha)) => Some(((size, mtime), sha)),
                        _ => None,
                    };
                    let recorded = Recorded {
                        id,
                        stamp: (size, mtime_ns),
                        sha256,
                        seen,
                    };
                    (recorded, state)
                },
            )
            .collect();
        let recorded: Vec<Recorded> = imports.iter().map(|(r, _)| r.clone()).collect();
        let at = path.clone();
        let found = tokio::task::spawn_blocking(move || check(Path::new(&at), &recorded))
            .await
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        for ((import, state), found) in imports.iter().zip(found) {
            if record(pool, &path, import, &found).await? && *state != found.state() {
                changed = true;
            }
        }
    }
    Ok(changed)
}

/// Records what a check found for `import`, unless a Locate re-pointed it meanwhile. Returns
/// whether the row was still the one checked.
async fn record(
    pool: &Pool<Sqlite>,
    path: &str,
    import: &Recorded,
    found: &Found,
) -> sqlx::Result<bool> {
    let (stamp, seen) = match found {
        Found::Same | Found::Missing => (import.stamp, None),
        Found::Restamped(stamp) => (*stamp, None),
        Found::Changed(stamp, sha256) => (import.stamp, Some((*stamp, sha256.as_str()))),
    };
    let updated = sqlx::query(
        "UPDATE imports SET original_state = ?, original_size = ?, original_mtime_ns = ?,
         seen_size = ?, seen_mtime_ns = ?, seen_sha256 = ?
         WHERE id = ? AND original_path = ? AND original_sha256 = ? AND original_size = ?
         AND original_mtime_ns = ?",
    )
    .bind(found.state())
    .bind(stamp.0)
    .bind(stamp.1)
    .bind(seen.map(|s| s.0 .0))
    .bind(seen.map(|s| s.0 .1))
    .bind(seen.map(|s| s.1))
    .bind(&import.id)
    .bind(path)
    .bind(&import.sha256)
    .bind(import.stamp.0)
    .bind(import.stamp.1)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// What Locate original did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum Located {
    /// Every import of the original now points at the file.
    Located,
    /// The file's sha256 differs from the original's, and nothing changed: it is used only once
    /// named by this sha256 (Use this file).
    Differs { path: String, sha256: String },
}

/// Why Locate original changed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocateRefused {
    /// No import of the library has this original.
    NoImport,
    /// A run for a pack of the original is queued or running, or a swap or Remove is journaled.
    Busy,
    /// The file isn't the one Use this file named: it changed since it was checked.
    NotNamed,
}

/// Re-points every import recorded at `original_path` at the file `path`, with its `stamp` and
/// `sha256`: when that is each import's sha256, or when `named` is it (Use this file), which
/// records it as the imports' new original, so every pack's next build reads it. Refused while
/// any pack of those imports is busy, so no build reads one original and records another.
pub async fn locate(
    pool: &Pool<Sqlite>,
    original_path: &str,
    path: &str,
    stamp: Stamp,
    sha256: &str,
    named: Option<&str>,
) -> sqlx::Result<Result<Located, LocateRefused>> {
    let mut tx = pool.begin().await?;
    let recorded: Vec<String> =
        sqlx::query_scalar("SELECT original_sha256 FROM imports WHERE original_path = ?")
            .bind(original_path)
            .fetch_all(&mut *tx)
            .await?;
    if recorded.is_empty() {
        return Ok(Err(LocateRefused::NoImport));
    }
    let same = recorded.iter().all(|r| r == sha256);
    match named {
        _ if same => {}
        Some(named) if named == sha256 => {}
        Some(_) => return Ok(Err(LocateRefused::NotNamed)),
        None => {
            return Ok(Ok(Located::Differs {
                path: path.to_string(),
                sha256: sha256.to_string(),
            }))
        }
    }
    let updated = sqlx::query(&format!(
        "UPDATE imports SET original_path = ?, original_size = ?, original_mtime_ns = ?,
         original_sha256 = ?, original_state = 'ok', seen_size = NULL, seen_mtime_ns = NULL,
         seen_sha256 = NULL WHERE original_path = ? AND NOT EXISTS (
         SELECT 1 FROM import_packs WHERE import_packs.import_id = imports.id AND {BUSY})"
    ))
    .bind(path)
    .bind(stamp.0)
    .bind(stamp.1)
    .bind(sha256)
    .bind(original_path)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != recorded.len() as u64 {
        return Ok(Err(LocateRefused::Busy));
    }
    tx.commit().await?;
    Ok(Ok(Located::Located))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dir: &Path, bytes: &[u8]) -> (std::path::PathBuf, Stamp, String) {
        let path = dir.join("pack.unitypackage");
        std::fs::write(&path, bytes).unwrap();
        let (size, mtime) = file_stamp(&path).unwrap();
        let sha256 = sha256_file(&path).unwrap();
        (path, (size as i64, mtime), sha256)
    }

    fn recorded(stamp: Stamp, sha256: &str, seen: Option<(Stamp, &str)>) -> Recorded {
        Recorded {
            id: "i".into(),
            stamp,
            sha256: sha256.into(),
            seen: seen.map(|(s, h)| (s, h.to_string())),
        }
    }

    #[test]
    fn an_original_with_its_recorded_stamp_is_not_hashed() {
        let dir = tempfile::tempdir().unwrap();
        let (path, stamp, _) = file(dir.path(), b"original");
        // A hash would find this sha256 wrong and answer Changed.
        let found = check(&path, &[recorded(stamp, "not its sha256", None)]);
        assert_eq!(found, [Found::Same]);
    }

    #[test]
    fn a_touched_original_is_hashed_and_keeps_its_new_stamp_when_its_sha256_matches() {
        let dir = tempfile::tempdir().unwrap();
        let (path, stamp, sha256) = file(dir.path(), b"original");
        let old = (stamp.0, stamp.1 - 1_000_000_000);
        let found = check(&path, &[recorded(old, &sha256, None)]);
        assert_eq!(found, [Found::Restamped(stamp)]);
        assert_eq!(found[0].state(), "ok");
    }

    #[test]
    fn a_gone_original_or_a_folder_in_its_place_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let r = recorded((8, 0), "sha", None);
        assert_eq!(
            check(&dir.path().join("gone.zip"), std::slice::from_ref(&r)),
            [Found::Missing]
        );
        assert_eq!(check(dir.path(), &[r]), [Found::Missing]);
    }

    #[test]
    fn a_changed_file_is_hashed_once_then_not_again_while_its_stamp_stays_the_one_seen() {
        let dir = tempfile::tempdir().unwrap();
        let (path, stamp, sha256) = file(dir.path(), b"changed");
        let old = (3, 1);
        // Two imports of the same original, one file: hashed once for both.
        let found = check(
            &path,
            &[recorded(old, "first", None), recorded(old, "second", None)],
        );
        let changed = Found::Changed(stamp, sha256.clone());
        assert_eq!(found, [changed.clone(), changed]);
        assert_eq!(found[0].state(), "changed");

        // Seen at this stamp: the sha256 kept is trusted, not the file hashed again.
        let found = check(&path, &[recorded(old, "first", Some((stamp, "kept")))]);
        assert_eq!(found, [Found::Changed(stamp, "kept".into())]);
        // Changed again since it was seen: hashed again.
        let found = check(&path, &[recorded(old, "first", Some(((1, 1), "kept")))]);
        assert_eq!(found, [Found::Changed(stamp, sha256)]);
    }
}
