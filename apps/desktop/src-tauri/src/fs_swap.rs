//! Staging a folder beside the one it replaces, then swapping it in by rename. Shared by the game
//! export's bundles (`export/bundle.rs`) and the pack import's builds (`import/build.rs`).
//!
//! A sibling is named `.<name>.<tag>-<id>` beside the folder `<name>`, so it sits on the same
//! filesystem, which makes the swap a rename, and the watcher never walks it (a dot folder).

use std::io;
use std::path::{Path, PathBuf};

/// The prefix of `folder`'s siblings tagged `tag`: `.<name>.<tag>-`.
pub fn prefix(folder: &Path, tag: &str) -> String {
    let name = folder
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!(".{name}.{tag}-")
}

/// The sibling `.<name>.<tag>-<id>` of `folder`.
pub fn sibling(folder: &Path, tag: &str, id: &str) -> PathBuf {
    parent(folder).join(format!("{}{id}", prefix(folder, tag)))
}

fn parent(folder: &Path) -> &Path {
    folder.parent().unwrap_or(Path::new("/"))
}

/// Every sibling of `folder` tagged `tag`, sorted. None when the folder holding `folder` is
/// missing.
pub fn leftovers(folder: &Path, tag: &str) -> io::Result<Vec<PathBuf>> {
    let prefix = prefix(folder, tag);
    let entries = match std::fs::read_dir(parent(folder)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        entries => entries?,
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// Creates the empty staging folder `sibling(folder, tag, id)`, and the folder holding `folder`
/// if needed.
pub fn create_staging(folder: &Path, tag: &str, id: &str) -> io::Result<PathBuf> {
    let staging = sibling(folder, tag, id);
    std::fs::create_dir_all(parent(folder))?;
    std::fs::create_dir(&staging)?;
    Ok(staging)
}

/// Deletes a staging or leftover folder, logging a failure. A missing one is no failure.
pub fn discard(folder: &Path) {
    match std::fs::remove_dir_all(folder) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("Could not delete the folder {}: {e}", folder.display()),
    }
}

/// Why `swap` failed.
#[derive(Debug)]
pub enum SwapError {
    /// Moving the live folder aside failed. Nothing moved.
    Aside(io::Error),
    /// Moving the staging folder in failed. With an `aside`, `restored` says whether the live
    /// folder was renamed back.
    In {
        error: io::Error,
        restored: Option<io::Result<()>>,
    },
}

/// What `swap` does with the folder it moved aside once the staging folder is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Old {
    Delete,
    /// Left for the caller, who deletes it once it has committed.
    Keep,
}

/// Swaps `staging` in as `folder` through `rename`. With `aside`, first renames `folder` there,
/// and renames it back when the second rename fails.
pub fn swap(
    folder: &Path,
    staging: &Path,
    aside: Option<&Path>,
    old: Old,
    mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> Result<(), SwapError> {
    if let Some(aside) = aside {
        rename(folder, aside).map_err(SwapError::Aside)?;
    }
    if let Err(error) = rename(staging, folder) {
        let restored = aside.map(|aside| rename(aside, folder));
        return Err(SwapError::In { error, restored });
    }
    if let (Some(aside), Old::Delete) = (aside, old) {
        // A leftover that the caller's recovery deletes
        if let Err(e) = std::fs::remove_dir_all(aside) {
            tracing::warn!(
                "Could not delete the previous folder {}: {e}",
                aside.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siblings_are_dot_folders_beside_the_folder() {
        let folder = Path::new("/lib/Pub/Pack");
        assert_eq!(
            sibling(folder, "import", "b1"),
            Path::new("/lib/Pub/.Pack.import-b1")
        );
    }

    #[test]
    fn a_kept_old_folder_stays_until_the_caller_deletes_it() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Pack");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("old.txt"), "old").unwrap();
        let staging = create_staging(&folder, "import", "b1").unwrap();
        std::fs::write(staging.join("new.txt"), "new").unwrap();
        let aside = sibling(&folder, "prev", "b1");

        swap(&folder, &staging, Some(&aside), Old::Keep, |a, b| {
            std::fs::rename(a, b)
        })
        .unwrap();

        assert!(folder.join("new.txt").exists());
        assert!(aside.join("old.txt").exists());
        assert_eq!(leftovers(&folder, "prev").unwrap(), [aside]);
        assert!(leftovers(&folder, "import").unwrap().is_empty());
    }

    #[test]
    fn a_failed_move_in_renames_the_old_folder_back() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Pack");
        std::fs::create_dir(&folder).unwrap();
        let staging = create_staging(&folder, "import", "b1").unwrap();
        let aside = sibling(&folder, "prev", "b1");

        let mut calls = 0;
        let error = swap(&folder, &staging, Some(&aside), Old::Keep, |a, b| {
            calls += 1;
            if calls == 2 {
                return Err(io::Error::other("injected"));
            }
            std::fs::rename(a, b)
        })
        .unwrap_err();

        assert!(matches!(
            error,
            SwapError::In {
                restored: Some(Ok(())),
                ..
            }
        ));
        assert!(folder.exists() && staging.exists() && !aside.exists());
    }
}
