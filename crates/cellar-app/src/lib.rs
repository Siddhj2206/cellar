//! The Cellar application layer (blueprint §4, ADR 0003): use-cases —
//! `FirstRunInstall`, `LaunchApp`, `DoctorCheck` — thin orchestration over
//! the `core` ports, generic for static dispatch and mock testing
//! (`PrefixService<S: Storage>`); `Box<dyn _>` appears only at the
//! composition root.
//!
//! This slice (#26) delivers the prefix lifecycle and the tree-health doctor
//! service. `FirstRunInstall` and `LaunchApp` land with their own slices
//! (#27+), as do the managed install and runner-integrity doctor sections.

pub mod services;

pub use services::{DoctorService, PrefixService};
