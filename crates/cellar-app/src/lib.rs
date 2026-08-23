//! The Cellar application layer (blueprint §4, ADR 0003): use-cases —
//! `FirstRunInstall`, `LaunchApp`, `DoctorCheck` — thin orchestration over
//! the `core` ports, generic for static dispatch and mock testing
//! (`PrefixService<S: Storage>`); `Box<dyn _>` appears only at the
//! composition root.
//!
//! This slice (#27) delivered the `AppEntry` registry (standalone install,
//! list with status, uninstall) on top of #26's prefix lifecycle and
//! tree-health doctor; the launch pipeline ships as `LaunchApp::plan`
//! (#28, dry-run) and `LaunchApp::spawn` (#29, execute — `--detach` and
//! per-launch logs are the presentation's policy); the install session's
//! other two artifact branches — installer (run inside the prefix, exit
//! awaited) and archive (extract into it, path-traversal-safe) — shipped
//! with the flat discovery scan in #30; the discovery review and
//! multi-registration land with #31 (`register_reviewed`: zero or more
//! entries, one prefix, never silent). `FirstRunInstall`'s interactive
//! session is presentation (the CLI's prompts over these use-cases), as
//! are the managed install and runner-integrity doctor sections.

pub mod archive;
pub mod services;

pub use archive::{ArchiveError, extract_zip};

pub use services::{
    ArtifactKind, DoctorService, EntryStatus, InstallError, InstallOutcome, InstallResult,
    InstallService, LaunchApp, ListedEntry, PrefixService,
};

// The launch surface presentations consume — through `app`, never directly
// (ADR 0002 third amendment: presentations consume `launch` through `app`).
pub use cellar_launch::{LaunchError, LaunchMode, SpawnedProcess};
