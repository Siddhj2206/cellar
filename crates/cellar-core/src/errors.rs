//! Error types for the port seam.
//!
//! The failure taxonomy (blueprint §7) cuts into five families — Resolve,
//! Check, Plan (pre-flight, doctor-flagged), Spawn, Runtime. This slice
//! carries the pre-flight port errors; Spawn/Runtime land with the execute
//! slice (#29).

use std::fmt;

/// Failure resolving a runner spec (pre-flight; doctor: `SuggestInstall`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No provider can service the spec (unknown family, order exhausted).
    ///
    /// Scaffold providers return this until #28 wires real resolution; the
    /// doc comments on each stub say so explicitly, so a pre-#28 failure is
    /// a stub, not a silent wiring bug.
    Unresolvable,
    /// The provider exists but its install is missing or corrupt
    /// (doctor: reinstall the runner).
    NotInstalled,
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unresolvable => write!(f, "no runner provider can resolve this spec"),
            Self::NotInstalled => write!(f, "the runner is not installed or is corrupt"),
        }
    }
}

impl std::error::Error for ResolveError {}

/// Failures of the storage port (file-tree CRUD, discovery, installer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    /// Underlying filesystem failure, with the offending path.
    Io(String),
    /// A file exists but is not valid. Hand-edited files degrade to a
    /// skipped entry flagged by doctor — never silently overwritten
    /// (ADR 0001).
    Invalid(String),
    /// The requested node does not exist.
    NotFound(String),
    /// The node already exists (slug clash without dedupe).
    Exists(String),
    /// A port method whose slice has not landed yet (e.g. discovery #27, the
    /// shared installer #34). Stubs return this with a loud message — never
    /// a silent success or a misleading taxonomy hit.
    Unimplemented(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path) => write!(f, "I/O failure at {path}"),
            Self::Invalid(path) => write!(f, "invalid file at {path}"),
            Self::NotFound(path) => write!(f, "not found: {path}"),
            Self::Exists(path) => write!(f, "already exists: {path}"),
            Self::Unimplemented(what) => write!(f, "not implemented yet: {what}"),
        }
    }
}

impl std::error::Error for StorageError {}

/// Failures of the desktop integration port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopError {
    /// Underlying filesystem failure, with the offending path.
    Io(String),
    /// The entry or association cannot be represented.
    Invalid(String),
}

impl fmt::Display for DesktopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path) => write!(f, "I/O failure at {path}"),
            Self::Invalid(what) => write!(f, "invalid desktop artifact: {what}"),
        }
    }
}

impl std::error::Error for DesktopError {}
