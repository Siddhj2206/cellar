//! The Cellar storage layer (blueprint §6, ADR 0001): the sole writer of the
//! file tree — TOML mapping, the flat discovery scan of the prefix's
//! menu/desktop areas, the shared installer pipeline, XDG path resolution —
//! implementing the `core::ports::Storage` seam. One external system, one
//! port.
//!
//! This slice (#26) delivered the tree: root resolution, first-run
//! initialization, settings and prefix/app file mapping, atomic writes, and
//! the tree-health report the doctor renders. The flat discovery scan lands
//! with #30 (`.lnk` target decoding follows with #31); the managed-runner
//! installer pipeline lands with #34 and currently returns the honest
//! `StorageError::Unimplemented`.

pub mod tree;

pub use tree::{SCHEMA_VERSION, TreeStore};
