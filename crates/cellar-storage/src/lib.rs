//! The Cellar storage layer (blueprint §6, ADR 0001): the sole writer of the
//! file tree — TOML mapping, `.lnk` discovery (the flat scan of the prefix's
//! menu/desktop areas plus the shell-link target decoding), the shared
//! installer pipeline, XDG path resolution — implementing the
//! `core::ports::Storage` seam. One external system, one port.
//!
//! This slice (#26) delivered the tree: root resolution, first-run
//! initialization, settings and prefix/app file mapping, atomic writes, and
//! the tree-health report the doctor renders. The flat discovery scan landed
//! with #30; `.lnk` target decoding with #31; the managed-runner installer
//! pipeline — fetch, verify, extract, flock, resumable cache, and the
//! authoritative `runtime/providers.toml` inventory — lands with #34
//! ([`installer`]). The disposable cache's retention sweep (#46) lands with
//! [`cache`].

pub mod cache;
mod installer;
mod lnk;
pub mod tree;

pub use cache::{LAUNCH_LOG_MIN_AGE, LAUNCH_LOGS_PER_SLUG};
pub use tree::{SCHEMA_VERSION, TreeStore};
