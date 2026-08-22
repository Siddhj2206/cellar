//! The Cellar storage layer (blueprint §6, ADR 0001): the sole writer of the
//! file tree — TOML mapping, `.lnk` discovery, the shared installer pipeline,
//! XDG path resolution — implementing the `core::ports::Storage` seam. One
//! external system, one port.
//!
//! This slice (#26) delivers the tree: root resolution, first-run
//! initialization, settings and prefix/app file mapping, atomic writes, and
//! the tree-health report the doctor renders. `.lnk` discovery and the
//! installer pipeline land with their own slices (#27, #34) and currently
//! return the honest `StorageError::Unimplemented`.

pub mod tree;

pub use tree::{SCHEMA_VERSION, TreeStore};
