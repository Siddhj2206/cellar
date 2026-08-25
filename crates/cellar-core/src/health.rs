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

/// The desktop-integration side of the doctor's report (#57): what the
/// launcher entries and the file association look like on this host —
/// read-only facts produced by the desktop adapter, rendered by the
/// doctor's fifth section. Icons are deliberately absent: the cache is
/// disposable (blueprint §6) and never worth a finding.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopIntegration {
    /// Every owned launcher entry whose Exec target no longer exists:
    /// `(slug, dead target)` pairs, sorted by slug. Staleness is
    /// existence-only — a target that exists but differs from the running
    /// binary is a legitimate multi-binary setup and is never reported.
    pub dead_entries: Vec<(String, PathBuf)>,
    /// The Open-with-Cellar association's state.
    pub association: AssociationState,
}

/// The file association's state as doctor sees it (#57).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AssociationState {
    /// No entries directory yet — the host was never integrated, so there
    /// is no derived artifact to be broken (a fresh tree passes clean).
    #[default]
    Untouched,
    /// Integration happened but the association file is absent, unreadable,
    /// or carries no Exec line to check.
    Missing,
    /// The association file exists but its Exec target does not.
    Dead(PathBuf),
    /// Present with an existing target.
    Wired,
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
