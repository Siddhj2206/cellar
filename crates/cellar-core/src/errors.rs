//! Error types for the port seam.
//!
//! The failure taxonomy (blueprint §7) cuts into five families — Resolve,
//! Check, Plan (pre-flight, doctor-flagged), Spawn, Runtime. This slice
//! carries the pre-flight port errors; Spawn/Runtime land with the execute
//! slice (#29).

use std::fmt;
use std::path::PathBuf;

use crate::types::{ProviderMode, RunnerFamily};

/// Why a resolution order found nothing — stamped by the provider that
/// services the spec's family, so fix hints derive from the provider's mode
/// (trait membership), never from family-string formatting at a render
/// site: managed families install through Cellar (`runner install`),
/// discover-only families come from the host (the system package manager,
/// or a configured path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnresolvedCause {
    /// The order found nothing anywhere — no valid configured path, no
    /// managed install, nothing on PATH. The servicing provider's mode
    /// names what an install looks like.
    NoneFound { mode: ProviderMode },
    /// A configured path was present but not an executable file before the
    /// order fell through — stale configuration, named exactly.
    StaleConfigured { path: PathBuf },
}

/// Failure resolving a runner spec (pre-flight; dispositions per blueprint
/// §7: order exhausted → doctor: `SuggestInstall`; corrupt install →
/// reinstall).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No provider can serve the spec — the family's resolution order
    /// (configured → managed → PATH, research #18) is exhausted, or no
    /// provider services the family. The failure's family names what a
    /// `SuggestInstall` must install; its cause carries why, driving the
    /// doctor's mode-derived fix hint.
    Unresolvable {
        family: RunnerFamily,
        cause: UnresolvedCause,
    },
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
            Self::Unresolvable { family, .. } | Self::NotInstalled { family } => *family,
        }
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unresolvable { family, .. } => write!(
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
    /// The environment misconfiguration (e.g. a relative `XDG_DATA_HOME`,
    /// #61) — presentation maps this to its usage-error exit code.
    Config(String),
    /// Underlying filesystem failure, with the offending path.
    Io(String),
    /// A file exists but is not valid, or a request names something that
    /// cannot be one. The payload is a COMPLETE sentence — this variant is
    /// rendered verbatim, with no prefix, so a message about a bad slug
    /// does not come out as "invalid file at invalid app slug" and one
    /// about a missing path is not misfiled as a file problem (#45).
    /// Hand-edited files degrade to a skipped entry flagged by doctor —
    /// never silently overwritten (ADR 0001).
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
            // Rendered verbatim: `Config` and `Invalid` payloads are whole
            // sentences, so no frame is bolted on. That is the #45 fix — the
            // old "invalid file at …" prefix made a validation sentence read
            // as "invalid file at invalid app slug …".
            Self::Config(what) | Self::Invalid(what) => write!(f, "{what}"),
            Self::Io(path) => write!(f, "I/O failure at {path}"),
            Self::NotFound(path) => write!(f, "not found: {path}"),
            Self::Exists(path) => write!(f, "already exists: {path}"),
            Self::Artifact(what) => write!(f, "artifact failure: {what}"),
            Self::Unimplemented(what) => write!(f, "not implemented yet: {what}"),
        }
    }
}

impl std::error::Error for StorageError {}

#[cfg(test)]
mod tests {
    use super::StorageError;

    /// AC (#45): `Invalid` is rendered verbatim, so a validation sentence
    /// reads as itself instead of being framed as a file problem.
    #[test]
    fn invalid_renders_its_payload_without_a_file_frame() {
        assert_eq!(
            StorageError::Invalid("invalid app slug \"Not A Slug!\"".to_owned()).to_string(),
            "invalid app slug \"Not A Slug!\"",
            "no doubled 'invalid file at invalid …'"
        );
        assert_eq!(
            StorageError::Invalid("cannot form a prefix slug from \"###\"".to_owned()).to_string(),
            "cannot form a prefix slug from \"###\"",
            "no ungrammatical 'invalid file at cannot form …'"
        );
        assert_eq!(
            StorageError::Invalid("/some/dir is not a file".to_owned()).to_string(),
            "/some/dir is not a file",
            "'file … is not a file' reads as written"
        );
    }

    #[test]
    fn every_invalid_payload_in_the_tree_reads_as_a_sentence() {
        // The verbatim contract on `Invalid` is only as good as the payloads
        // flowing into it, and nothing in the type enforces that (#45). This
        // walks the real storage call sites through the real renderer: a
        // payload that regresses to a bare path or a fragment fails here
        // rather than in a user's terminal.
        let sentences = [
            "invalid app slug \"Not A Slug!\"",
            "cannot form a prefix slug from \"###\"",
            "/some/dir is not a file",
            "/tmp/thing: unparseable header (bad TOML)",
            "/tmp/prefix.toml: schema version 9 (the tree reads 1)",
        ];
        for payload in sentences {
            let rendered = StorageError::Invalid(payload.to_owned()).to_string();
            assert_eq!(
                rendered, payload,
                "rendered verbatim, so the payload must stand alone"
            );
            assert!(
                !rendered.starts_with("invalid file at"),
                "the frame is gone (#45): {rendered}"
            );
        }
    }
}

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
