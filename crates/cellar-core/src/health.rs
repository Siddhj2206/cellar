//! Tree health reporting (blueprint §6, §7): the doctor's source of truth —
//! which tree nodes are missing, which files are invalid, which prefix dirs
//! are orphaned. Pure data, produced by the storage adapter, rendered by
//! presentation.

use serde::{Deserialize, Serialize};

use std::path::PathBuf;

/// A health report of the storage tree (blueprint §7: the doctor is the check
/// phase applied tree-wide). Paths are relative to the tree root so reports
/// stay readable and machine-friendly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeHealth {
    /// The tree root.
    pub root: PathBuf,
    /// Whether the root directory exists at all.
    pub tree_exists: bool,
    /// Required top-level directories missing from the tree.
    pub missing_dirs: Vec<PathBuf>,
    /// Required files missing while the tree exists (e.g. a deleted
    /// `settings.toml`).
    pub missing_files: Vec<PathBuf>,
    /// Files that exist but cannot be parsed at the locked schema version.
    /// Hand-edited files land here — skipped by reads, never overwritten
    /// (ADR 0001).
    pub invalid_files: Vec<PathBuf>,
    /// Prefix directories without a `prefix.toml` (debris or an interrupted
    /// create).
    pub orphan_prefix_dirs: Vec<PathBuf>,
    /// App slugs whose registered exe is missing from disk (deleted or
    /// moved). File-oriented lists above use tree-relative paths; entry
    /// checks use slugs — entry identity is domain vocabulary, file layout
    /// is storage's.
    pub missing_exes: Vec<String>,
    /// The schema version the tree is read at.
    pub schema_version: u32,
}

impl TreeHealth {
    /// Whether the tree is healthy: present, complete, every file valid, and
    /// every registered exe still on disk (blueprint §7 check phase applied
    /// tree-wide).
    pub fn is_healthy(&self) -> bool {
        self.tree_exists
            && self.missing_dirs.is_empty()
            && self.missing_files.is_empty()
            && self.invalid_files.is_empty()
            && self.orphan_prefix_dirs.is_empty()
            && self.missing_exes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::TreeHealth;

    use std::path::PathBuf;

    fn report() -> TreeHealth {
        TreeHealth {
            root: PathBuf::from("/tmp/cellar"),
            tree_exists: true,
            missing_dirs: Vec::new(),
            missing_files: Vec::new(),
            invalid_files: Vec::new(),
            orphan_prefix_dirs: Vec::new(),
            missing_exes: Vec::new(),
            schema_version: 1,
        }
    }

    #[test]
    fn missing_registered_exes_make_the_tree_unhealthy() {
        let mut health = report();
        assert!(health.is_healthy());
        health.missing_exes.push("balatro".to_owned());
        assert!(!health.is_healthy());
    }
}
