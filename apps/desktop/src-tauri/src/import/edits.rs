//! In-app renames and deletes inside an imported pack (plan pack-import, Edits after an import):
//! `rename_asset`, `delete_asset` and `delete_assets` record each as a step, so a rebuild keeps it.
//!
//! - **A file with a source path** (extracted, or kept in place of one) gets a `rename` or an
//!   `exclude` by its source path. The step covers its `.meta`, which the command renames or
//!   deletes on disk beside it (`prepare`'s sidecars).
//! - **A generated GLB** gets no step: it is in a format group with its source model, whose own
//!   step covers it.
//! - **The pack's `datapackage.json`** can't be renamed, and a delete turns the metadata step off.
//! - **A kept file added outside OpenDAM** gets no step.
//!
//! Every file's manifest row follows it, and the pack's `recipe_rev` goes up, in the command's
//! own transaction, refused while a run for the pack is queued or running.

use std::path::Path;

use sqlx::{Pool, Sqlite, SqliteConnection};

use super::engine::BUSY as BUILDING;
use super::place::PlaceStep;
use super::records::{Refused, StepRecord, BUSY};
use crate::structure::is_package_file;

/// The imported pack whose target holds the path `?1`, on whole path components: its id,
/// target, current build and whether it is busy. Targets never nest, so there is at most one.
fn pack_of() -> String {
    format!(
        "SELECT id, target, current_build, {BUSY} FROM import_packs
         WHERE substr(?1, 1, length(target) + 1) = target || '/'"
    )
}

type Pack = (String, String, Option<String>, bool);

/// Whether `path` is the `datapackage.json` (any ASCII case) right in the pack folder `target`.
fn is_pack_package(target: &str, path: &str) -> bool {
    path.strip_prefix(target)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| !name.contains('/') && is_package_file(name))
}

/// Checks a rename (`renaming`) or a delete of `paths`, relative to the library root `root`,
/// before anything changes. Refused, with the message, when one is in a pack a run is building,
/// or for a rename, when one is a pack's `datapackage.json`. Otherwise returns, for each path,
/// the `.meta` that goes with it: the current build's, still a file on disk, and not itself in
/// `paths`.
pub async fn prepare(
    pool: &Pool<Sqlite>,
    root: &Path,
    paths: &[&str],
    renaming: bool,
) -> sqlx::Result<Result<Vec<Option<String>>, String>> {
    let mut sidecars = Vec::new();
    for path in paths {
        let pack: Option<Pack> = sqlx::query_as(&pack_of())
            .bind(path)
            .fetch_optional(pool)
            .await?;
        let Some((pack_id, target, build, busy)) = pack else {
            sidecars.push(None);
            continue;
        };
        if busy {
            return Ok(Err(BUILDING.to_string()));
        }
        let Some(build) = build else {
            sidecars.push(None);
            continue;
        };
        if renaming && is_pack_package(&target, path) {
            return Ok(Err(format!(
                "{path} is written by the pack's import, so it can't be renamed: edit the \
                 pack's metadata step instead"
            )));
        }
        let meta = format!("{path}.meta");
        let listed: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM import_outputs WHERE pack_id = ? AND build_id = ? AND path = ?",
        )
        .bind(&pack_id)
        .bind(&build)
        .bind(&meta)
        .fetch_optional(pool)
        .await?;
        let on_disk = root
            .join(&meta)
            .symlink_metadata()
            .is_ok_and(|m| m.is_file());
        let goes = listed.is_some() && on_disk && !paths.contains(&meta.as_str());
        sidecars.push(goes.then_some(meta));
    }
    Ok(Ok(sidecars))
}

/// A file a rename or a delete changed, by its path relative to the library root.
#[derive(Debug, Clone)]
pub struct Change {
    pub path: String,
    /// The new path of a renamed file, `None` for a deleted one.
    pub to: Option<String>,
    /// A `.meta` that went with its file: its row follows, and its file's step covers it.
    pub sidecar: bool,
}

/// Records `changes` in `tx`, the command's own transaction. In each imported pack with a
/// current build they touch, it bumps `recipe_rev` once, appends a `rename` or an `exclude` for
/// each renamed or deleted file of that build with a source path, other than a generated file
/// or a sidecar, turns the metadata step off for a deleted `datapackage.json`, and moves or drops
/// each file's manifest row. Refused when a run for one of the packs is queued or running, and
/// the caller then rolls `tx` back.
pub async fn record(
    tx: &mut SqliteConnection,
    changes: &[Change],
) -> sqlx::Result<Result<(), Refused>> {
    let mut packs: Vec<(String, String, String, Vec<&Change>)> = Vec::new();
    for change in changes {
        if change.to.as_ref() == Some(&change.path) {
            continue;
        }
        let pack: Option<Pack> = sqlx::query_as(&pack_of())
            .bind(&change.path)
            .fetch_optional(&mut *tx)
            .await?;
        let Some((pack_id, target, Some(build), _)) = pack else {
            continue;
        };
        match packs.iter_mut().find(|p| p.0 == pack_id) {
            Some(pack) => pack.3.push(change),
            None => packs.push((pack_id, target, build, vec![change])),
        }
    }

    for (pack_id, target, build, changes) in packs {
        let bumped: Option<i64> = sqlx::query_scalar(&format!(
            "UPDATE import_packs SET recipe_rev = recipe_rev + 1 WHERE id = ? AND NOT {BUSY}
             RETURNING recipe_rev"
        ))
        .bind(&pack_id)
        .fetch_optional(&mut *tx)
        .await?;
        if bumped.is_none() {
            return Ok(Err(Refused::Busy));
        }
        let mut position: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM import_steps WHERE pack_id = ?",
        )
        .bind(&pack_id)
        .fetch_one(&mut *tx)
        .await?;
        for change in changes {
            let row: Option<(String, Option<String>)> = sqlx::query_as(
                "SELECT origin, source_path FROM import_outputs
                 WHERE pack_id = ? AND build_id = ? AND path = ?",
            )
            .bind(&pack_id)
            .bind(&build)
            .bind(&change.path)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((origin, source)) = row else {
                continue;
            };
            let follow = match &change.to {
                Some(to) => sqlx::query(
                    "UPDATE import_outputs SET path = ?4
                     WHERE pack_id = ?1 AND build_id = ?2 AND path = ?3",
                )
                .bind(&pack_id)
                .bind(&build)
                .bind(&change.path)
                .bind(to),
                None => sqlx::query(
                    "DELETE FROM import_outputs WHERE pack_id = ? AND build_id = ? AND path = ?",
                )
                .bind(&pack_id)
                .bind(&build)
                .bind(&change.path),
            };
            follow.execute(&mut *tx).await?;
            if change.sidecar {
                continue;
            }
            if change.to.is_none() && is_pack_package(&target, &change.path) {
                sqlx::query(
                    "UPDATE import_steps SET enabled = 0 WHERE pack_id = ? AND kind = 'metadata'",
                )
                .bind(&pack_id)
                .execute(&mut *tx)
                .await?;
            }
            let source: Vec<String> = source
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            if origin == "generated" || source.is_empty() {
                continue;
            }
            let step = match &change.to {
                Some(to) => PlaceStep::Rename {
                    source,
                    name: to.rsplit('/').next().unwrap_or(to).to_string(),
                },
                None => PlaceStep::Exclude { source },
            };
            let step = StepRecord::place(&step);
            sqlx::query(
                "INSERT INTO import_steps (pack_id, position, kind, enabled, options)
                 VALUES (?, ?, ?, 1, ?)",
            )
            .bind(&pack_id)
            .bind(position)
            .bind(&step.kind)
            .bind(step.options.to_string())
            .execute(&mut *tx)
            .await?;
            position += 1;
        }
    }
    Ok(Ok(()))
}
