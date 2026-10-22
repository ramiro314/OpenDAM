//! Source file hashes, memoized in each library's `file_hashes` table (plan game-export A4), so a
//! check re-hashes only the files whose size or mtime changed since it last hashed them.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};
use sqlx::{Pool, Sqlite};

use crate::structure::BATCH;

/// A file's size and mtime (nanoseconds since the epoch) as a hash was taken at, and the hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hashed {
    pub size_bytes: i64,
    pub modified_at: i64,
    pub sha256: String,
}

/// The memo rows of one library for the files a check reads, and the hashes it took since.
#[derive(Debug, Default)]
pub struct Memo {
    known: HashMap<String, Hashed>,
    /// Hashes taken by `sha256`, to save with `save`
    pub fresh: Vec<(String, Hashed)>,
}

/// The memo rows of `paths` (relative to the library root) in the library DB at `pool`.
pub async fn load(pool: &Pool<Sqlite>, paths: &[String]) -> sqlx::Result<Memo> {
    let mut known = HashMap::new();
    for chunk in paths.chunks(BATCH) {
        let rows: Vec<(String, i64, i64, String)> = sqlx::query_as(
            "SELECT file_path, size_bytes, modified_at, sha256 FROM file_hashes
             WHERE file_path IN (SELECT value FROM json_each(?))",
        )
        .bind(serde_json::to_string(chunk).expect("paths serialize"))
        .fetch_all(pool)
        .await?;
        known.extend(
            rows.into_iter()
                .map(|(path, size_bytes, modified_at, sha256)| {
                    (
                        path,
                        Hashed {
                            size_bytes,
                            modified_at,
                            sha256,
                        },
                    )
                }),
        );
    }
    Ok(Memo {
        known,
        fresh: Vec::new(),
    })
}

/// The lowercase hex sha256 of the file at `path`.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

impl Memo {
    /// The sha256 of `file_path` under `root`: the memoized one while the file's size and mtime
    /// are those it was taken at, else a fresh one, which `fresh` keeps for `save`.
    pub fn sha256(&mut self, root: &Path, file_path: &str) -> std::io::Result<String> {
        let absolute = root.join(file_path);
        let meta = std::fs::metadata(&absolute)?;
        let size_bytes = meta.len() as i64;
        let modified_at = meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as i64);
        if let Some(known) = self.known.get(file_path) {
            if known.size_bytes == size_bytes && known.modified_at == modified_at {
                return Ok(known.sha256.clone());
            }
        }
        let hashed = Hashed {
            size_bytes,
            modified_at,
            sha256: sha256_file(&absolute)?,
        };
        self.known.insert(file_path.to_string(), hashed.clone());
        self.fresh.push((file_path.to_string(), hashed.clone()));
        Ok(hashed.sha256)
    }
}

/// Saves `fresh` hashes in transactions of `BATCH` rows, replacing older rows of the same files.
pub async fn save(pool: &Pool<Sqlite>, fresh: &[(String, Hashed)]) -> sqlx::Result<()> {
    for chunk in fresh.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for (path, hashed) in chunk {
            sqlx::query(
                "INSERT INTO file_hashes (file_path, size_bytes, modified_at, sha256)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(file_path) DO UPDATE SET size_bytes = excluded.size_bytes,
                     modified_at = excluded.modified_at, sha256 = excluded.sha256",
            )
            .bind(path)
            .bind(hashed.size_bytes)
            .bind(hashed.modified_at)
            .bind(&hashed.sha256)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memory_library_pool;

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[tokio::test]
    async fn hashes_are_memoized_until_the_file_changes() {
        let pool = memory_library_pool().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rock.png"), "abc").unwrap();
        let paths = vec!["rock.png".to_string()];

        let mut memo = load(&pool, &paths).await.unwrap();
        assert_eq!(memo.sha256(dir.path(), "rock.png").unwrap(), ABC);
        assert_eq!(memo.fresh.len(), 1);
        save(&pool, &memo.fresh).await.unwrap();

        // A memoized row is used while the size and mtime match, even if the bytes don't
        sqlx::query("UPDATE file_hashes SET sha256 = 'memo'")
            .execute(&pool)
            .await
            .unwrap();
        let mut memo = load(&pool, &paths).await.unwrap();
        assert_eq!(memo.sha256(dir.path(), "rock.png").unwrap(), "memo");
        assert!(memo.fresh.is_empty());

        // A changed size hashes again, and saving replaces the row
        std::fs::write(dir.path().join("rock.png"), "abcd").unwrap();
        let fresh = memo.sha256(dir.path(), "rock.png").unwrap();
        assert_ne!(fresh, "memo");
        save(&pool, &memo.fresh).await.unwrap();
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT file_path, sha256 FROM file_hashes")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, [("rock.png".to_string(), fresh)]);
    }

    #[tokio::test]
    async fn a_missing_file_is_an_error() {
        let pool = memory_library_pool().await;
        let dir = tempfile::tempdir().unwrap();
        let mut memo = load(&pool, &[]).await.unwrap();

        assert!(memo.sha256(dir.path(), "gone.png").is_err());
    }
}
