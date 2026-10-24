//! Rows follow their files (plan pack-import, Flow step 5 and Rollback). A rebuild carries each
//! asset row whose file it places at another path to that path, with its id and metadata, and a
//! rollback carries it back.
//!
//! The previous build's manifest and the new one are joined on the **file key**: `source_path`
//! plus whether the file is generated. Each carry runs in two passes, so the order of the moves
//! never matters, not even when two files swap names:
//!
//! - **Aside.** Every row that moves goes to a holding path, `<pack>/.opendam-carry/<asset id>`,
//!   which no file has and no index walks. Each move is journaled in `import_moves` as `moved`
//!   in the transaction of its aside, 200 at a time, before any row is placed.
//! - **Place.** Each row then goes from its holding path to its new path.
//!
//! A row at a path of the new build whose file there has another file key than the previous
//! build's file there, or where the previous build had none, belongs to a file the new build
//! doesn't have there: a previous-only file, or one that moved and is carried itself. One that
//! isn't moving is moved aside too (journaled as `moved` to its holding path) and stays there, so
//! the settle deletes it and a rollback puts it back. That covers the paths of carried files and
//! of files new to this build alike (one whose `exclude` was turned off, say). So no carry is
//! skipped, and no file takes over another file's row.

use std::collections::HashSet;
use std::path::Path;

use sqlx::{Pool, Sqlite, SqliteConnection};

use crate::models::AssetKind;

/// Rows per transaction, as `watcher.rs` writes them.
const BATCH: usize = 200;

/// Where row `asset_id` of the pack at `target` waits between the two passes.
pub fn holding(target: &str, asset_id: &str) -> String {
    format!("{target}/.opendam-carry/{asset_id}")
}

/// One row to move: from where it is to where it goes. `to` is its holding path for a row that
/// stays aside.
struct Move {
    asset_id: String,
    from: String,
    to: String,
    /// Placed after the aside pass. A displaced row isn't.
    placed: bool,
}

/// Carries the rows of the files build `build_id` places at other paths than build `prev` did,
/// journaling each move under pack `pack_id`'s build `build_id`, from `seq` 1. Run it before
/// the build's index, whose journal of inserted rows continues after these.
pub async fn carry(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    target: &str,
    prev: &str,
    build_id: &str,
) -> sqlx::Result<()> {
    // Each moved file's row.
    let carried: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT a.id, p.path, n.path FROM import_outputs p
         JOIN import_outputs n ON n.pack_id = p.pack_id AND n.build_id = ?3
             AND n.source_path = p.source_path
             AND (n.origin = 'generated') = (p.origin = 'generated')
         JOIN assets a ON a.file_path = p.path
         WHERE p.pack_id = ?1 AND p.build_id = ?2 AND p.source_path IS NOT NULL
             AND p.path != n.path
         ORDER BY p.path",
    )
    .bind(pack_id)
    .bind(prev)
    .bind(build_id)
    .fetch_all(pool)
    .await?;
    // Each row at a path of the new build that the previous build's same file didn't hold.
    let occupied: Vec<(String, String)> = sqlx::query_as(
        "SELECT d.id, n.path FROM import_outputs n
         JOIN assets d ON d.file_path = n.path
         LEFT JOIN import_outputs p ON p.pack_id = n.pack_id AND p.build_id = ?2
             AND p.path = n.path
         WHERE n.pack_id = ?1 AND n.build_id = ?3
             AND (p.path IS NULL OR p.source_path IS NOT n.source_path
                 OR (p.origin = 'generated') != (n.origin = 'generated'))
         ORDER BY n.path",
    )
    .bind(pack_id)
    .bind(prev)
    .bind(build_id)
    .fetch_all(pool)
    .await?;
    let moving: HashSet<&str> = carried.iter().map(|(id, ..)| id.as_str()).collect();
    let displaced = occupied
        .iter()
        .filter(|(id, _)| !moving.contains(id.as_str()))
        .map(|(id, path)| Move {
            asset_id: id.clone(),
            from: path.clone(),
            to: holding(target, id),
            placed: false,
        });
    let moves: Vec<Move> = displaced
        .chain(carried.iter().map(|(id, from, to)| Move {
            asset_id: id.clone(),
            from: from.clone(),
            to: to.clone(),
            placed: true,
        }))
        .collect();

    let mut seq = 0;
    for chunk in moves.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for m in chunk {
            seq += 1;
            sqlx::query(
                "INSERT INTO import_moves (pack_id, build_id, seq, kind, asset_id, from_path,
                 to_path) VALUES (?, ?, ?, 'moved', ?, ?, ?)",
            )
            .bind(pack_id)
            .bind(build_id)
            .bind(seq)
            .bind(&m.asset_id)
            .bind(&m.from)
            .bind(&m.to)
            .execute(&mut *tx)
            .await?;
            set_aside(&mut tx, target, &m.asset_id).await?;
        }
        tx.commit().await?;
    }
    let placed: Vec<&Move> = moves.iter().filter(|m| m.placed).collect();
    for chunk in placed.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for m in chunk {
            place(&mut tx, &m.asset_id, &m.to).await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

/// Reverses build `build_id`'s `moved` journal entries, by asset id, wherever each row is now:
/// every row aside first, then each back at its old path. A row whose old path is taken anyway
/// stays aside, and the conflict is logged: a rollback never fails on it. Run it after the rows
/// the build's index inserted are deleted, so their paths are free.
pub async fn reverse(
    pool: &Pool<Sqlite>,
    pack_id: &str,
    target: &str,
    build_id: &str,
) -> sqlx::Result<()> {
    let entries: Vec<(String, String)> = sqlx::query_as(
        "SELECT asset_id, from_path FROM import_moves
         WHERE pack_id = ? AND build_id = ? AND kind = 'moved' ORDER BY seq",
    )
    .bind(pack_id)
    .bind(build_id)
    .fetch_all(pool)
    .await?;
    for chunk in entries.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for (asset_id, _) in chunk {
            set_aside(&mut tx, target, asset_id).await?;
        }
        tx.commit().await?;
    }
    for chunk in entries.chunks(BATCH) {
        let mut tx = pool.begin().await?;
        for (asset_id, from) in chunk {
            let taken: Option<String> =
                sqlx::query_scalar("SELECT id FROM assets WHERE file_path = ?")
                    .bind(from)
                    .fetch_optional(&mut *tx)
                    .await?;
            if let Some(other) = taken {
                tracing::error!(
                    "Rolling back build {build_id}: {from} holds row {other}, so row {asset_id} \
                     stays at {}",
                    holding(target, asset_id)
                );
                continue;
            }
            place(&mut tx, asset_id, from).await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

/// Moves row `asset_id` to its holding path, changing nothing else.
async fn set_aside(tx: &mut SqliteConnection, target: &str, asset_id: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE assets SET file_path = ? WHERE id = ?")
        .bind(holding(target, asset_id))
        .bind(asset_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

/// Moves row `asset_id` to `path`, as `rename_asset` does, keeping its `modified_at` so the
/// index sees an unchanged file there as unchanged. The FTS update trigger follows it.
async fn place(tx: &mut SqliteConnection, asset_id: &str, path: &str) -> sqlx::Result<()> {
    let name = path.rsplit_once('/').map_or(path, |(_, name)| name);
    let extension = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    sqlx::query(
        "UPDATE assets SET file_name = ?, file_path = ?, extension = ?, kind = ? WHERE id = ?",
    )
    .bind(name)
    .bind(path)
    .bind(&extension)
    .bind(AssetKind::from_extension(&extension).as_str())
    .bind(asset_id)
    .execute(&mut *tx)
    .await?;
    Ok(())
}
