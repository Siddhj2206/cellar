//! Crash-durability helpers (#62): an atomic write is only half the
//! story — after a rename, the directory entry itself must reach the
//! disk, or a power cut reverts a recorded fact to absent. One shared
//! helper for every crate that atomically writes tree metadata, desktop
//! entries, or install directories (ADR 0002: all depend on core).

use std::path::Path;

/// fsync the directory that contains `path` — the rename/entry-creation
/// durability half of an atomic write (#62). Unix-only: on other
/// platforms this is a no-op (ADR 0003: Linux-first, a port decides).
pub fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::fs::File;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Recursively `sync_all` every file and directory under `dir`, then the
/// directory itself — the post-extraction sweep before a managed-runner
/// install root is renamed into place (#62). One-time O(n) next to a
/// multi-gigabyte download.
pub fn sync_tree(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            std::fs::File::open(entry.path())?.sync_all()?;
        }
    }
    #[cfg(unix)]
    {
        use std::fs::File;
        File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}
