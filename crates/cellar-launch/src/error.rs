//! Failures of the launch pipeline, mapped to the blueprint §7 failure
//! taxonomy and its dispositions — the pre-flight families (Resolve, Check,
//! Plan) and the execute phase's post-plan Spawn family (the OS error,
//! reported as-is). The Runtime family (the exit code) is propagated raw by
//! the presentation, not this error type.
//!
//! This slice (#28) carried the resolve and check families plus the
//! plan-family "not planable yet" boundary; the spawn family lands with the
//! execute slice (#29).

use cellar_core::errors::{ResolveError, StorageError};
use cellar_core::types::RunnerFamily;

use std::fmt;
use std::path::PathBuf;

/// A launch that failed. Pre-plan variants end in the blueprint §7
/// disposition — the fix, not just the failure; the post-plan Spawn family
/// reports the OS error as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    /// No entry is registered under this slug.
    AppNotFound { slug: String },
    /// The bound prefix directory is gone — recreate it. Cellar never
    /// auto-recreates: hand-edited state is never clobbered (ADR 0001).
    PrefixMissing { slug: String },
    /// The bound prefix directory exists but `prefix.toml` is missing or
    /// unreadable (hand-edit damage): `prefix create` cannot repair it —
    /// the dir occupies the slug in the dedupe domain — so the exact file
    /// is named for a hand fix or removal (#51, ADR 0001).
    PrefixDamaged { slug: String },
    /// Runner resolution failed — unknown spec / order exhausted →
    /// `SuggestInstall`, install missing → reinstall (the disposition text
    /// lives in `ResolveError`'s messages).
    Resolve(ResolveError),
    /// The registered exe is gone or no longer a file — re-register the
    /// entry or uninstall it.
    ExeMissing { slug: String, exe: PathBuf },
    /// The resolved runner cannot be planned yet — the wrapper chain for
    /// this family (the umu container layer) lands with the managed-runner
    /// pipeline (#34).
    PlanUnavailable { family: RunnerFamily },
    /// The execute phase failed to start or capture the plan's process
    /// (blueprint §7 Spawn family, post-plan): the OS error is reported
    /// as-is, named with the program the launch ran.
    Spawn { program: String, error: String },
    /// Any other storage failure (I/O, invalid tree files) — surfaced in
    /// the tree's own vocabulary.
    Storage(StorageError),
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AppNotFound { slug } => write!(
                f,
                "no app '{slug}' is registered — register it with `cellar install <path>`"
            ),
            Self::PrefixMissing { slug } => write!(
                f,
                "the prefix '{slug}' this app binds to is missing — \
                 recreate it with `cellar prefix create {slug}`"
            ),
            Self::PrefixDamaged { slug } => write!(
                f,
                "the prefix '{slug}' this app binds to is damaged (its prefix.toml is \
                 missing or unreadable) — fix or remove `prefixes/{slug}/prefix.toml` by hand"
            ),
            Self::Resolve(err) => write!(f, "{err}"),
            Self::ExeMissing { slug, exe } => write!(
                f,
                "the registered exe of '{slug}' is gone ({} — the file is missing \
                 or not a file) — re-register it with `cellar install` or uninstall \
                 the entry",
                exe.display()
            ),
            Self::PlanUnavailable { family } => write!(
                f,
                "cannot plan a {} launch yet — the wrapper chain for this family \
                 lands with the managed-runner pipeline (#34)",
                family.as_str()
            ),
            Self::Spawn { program, error } => {
                write!(f, "failed to launch {program}: {error}")
            }
            Self::Storage(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for LaunchError {
    // No `source` links: every variant's `Display` is the complete message
    // including its disposition, so an `{err:#}` chain must not restate
    // wrapped errors (the CLI prints exactly one actionable line).
}
