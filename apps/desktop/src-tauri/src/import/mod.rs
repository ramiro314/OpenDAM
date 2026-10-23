//! Pack import from originals (docs/plans/pack-import/plan.md).

pub mod place;
pub mod unitypackage;

use sqlx::{Pool, Sqlite};
use std::path::Path;

/// Which swap recovery tables a caller runs (plan pack-import, "Swap recovery").
#[derive(Clone, Copy, Debug)]
pub enum Recovery {
    /// A library opening: packs left `swapping`, then the leftovers of packs with no live run.
    Open,
    /// The start of a locking index or structure entry point: packs left `swapping` only.
    Entry,
}

/// Swap recovery for the library DB at `pool`, whose files lie below `root`. Its callers hold
/// the library's `index_lock` and advisory lock and call it before they touch a row: the library
/// open (`db::open_library_db`), `watcher::index_paths` and `structure::rebuild_library`, its
/// follow-up task included. No build swaps a pack yet, so there is nothing to recover: the
/// import build fills this in.
pub async fn recover_swaps(_pool: &Pool<Sqlite>, _root: &Path, _tables: Recovery) {}
