//! The Cellar application layer (blueprint §4, ADR 0003): use-cases —
//! `FirstRunInstall`, `LaunchApp`, `DoctorCheck` — thin orchestration over
//! the `core` ports, generic for static dispatch and mock testing
//! (`PrefixService<S: Storage>`); `Box<dyn _>` appears only at the
//! composition root.
//!
//! This slice (#27) delivers the `AppEntry` registry (standalone install, list
//! with status, uninstall) on top of #26's prefix lifecycle and tree-health
//! doctor. `FirstRunInstall`'s interactive session and `LaunchApp` land with
//! their own slices (#30+), as do the managed install and runner-integrity
//! doctor sections.

pub mod services;

pub use services::{
    DoctorService, EntryStatus, InstallResult, InstallService, ListedEntry, PrefixService,
};
