//! The Cellar storage layer (blueprint §6, ADR 0001): the sole writer of the
//! file tree — TOML mapping, `.lnk` discovery, the shared installer pipeline,
//! XDG path resolution — implementing the `core::ports::Storage` seam. One
//! external system, one port.
//!
//! Lands with slice 02 (#26: storage tree and prefix management). The
//! scaffold ships the crate so every later slice starts from a wired,
//! rule-enforced workspace.
