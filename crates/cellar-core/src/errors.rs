//! Error types for the port seam.
//!
//! The failure taxonomy (blueprint §7) cuts into five families — Resolve,
//! Check, Plan (pre-flight, doctor-flagged), Spawn, Runtime. This slice
//! carries the pre-flight port errors; Spawn/Runtime land with the execute
//! slice (#29).

use std::fmt;

use crate::types::RunnerFamily;

/// Failure resolving a runner spec (pre-flight; dispositions per blueprint
/// §7: order exhausted → doctor: `SuggestInstall`; corrupt install →
/// reinstall).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No provider can serve the spec — the family's resolution order
    /// (configured → managed → PATH, research #18) is exhausted, or no
    /// provider services the family. The failure's family names what a
    /// `SuggestInstall` must install.
    Unresolvable { family: RunnerFamily },
    /// The provider exists but its install is missing or corrupt
    /// (doctor: reinstall the runner).
    NotInstalled { family: RunnerFamily },
}

impl ResolveError {
    /// The family the failure concerns — the family of the spec being
    /// resolved. A composition of providers keeps the serviced family's
    /// error over another family's "not me" answer (#28).
    pub const fn family(&self) -> RunnerFamily {
        match self {
            Self::Unresolvable { family } | Self::NotInstalled { family } => *family,
        }
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unresolvable { family } => write!(
                f,
                "no {} runner could be resolved — install it or configure a path",
                family.as_str()
            ),
            Self::NotInstalled { family } => write!(
                f,
                "the {} runner is not installed or is corrupt — reinstall it",
                family.as_str()
            ),
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
    /// A shared-pipeline artifact failure — the managed-installer pipeline
    /// (fetch, checksum, extraction, layout, probe): the artifact could not
    /// be acquired as declared. The message names the failing step.
    Artifact(String),
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
            Self::Artifact(what) => write!(f, "artifact failure: {what}"),
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
