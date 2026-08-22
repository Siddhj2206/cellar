//! Declarative managed-runner descriptors (blueprint §5, research #18).

use serde::{Deserialize, Serialize};

/// How a managed runner is acquired and installed: one descriptor drives the
/// storage-owned installer pipeline. Adding a managed runner is one descriptor
/// and one registry line — zero pipeline code (ADR 0003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerManifest {
    /// Stable provider identifier, e.g. "proton", "umu".
    ///
    /// Deliberately stringly typed: provider additions must never touch
    /// `core` (blueprint §4), so identifiers cannot be a closed enum.
    pub provider_id: String,
    pub source: ReleaseSource,
    pub checksum: ChecksumScheme,
    pub archive: ArchiveLayout,
    pub install_kind: InstallKind,
}

/// Templates for building release artifact URLs. `{tag}` is the version and
/// `{arch}` the target architecture (e.g. `x86_64`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseSource {
    pub url_template: String,
    /// Checksum file for the artifact, when published separately
    /// (e.g. `<tag>-<arch>.sha512sum`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_url_template: Option<String>,
}

/// Checksum algorithm used for verification. Research #18 corrected the
/// ticket's "sha256": GE-Proton publishes SHA-512, and Cellar verifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChecksumScheme {
    Sha512,
}

/// How the artifact extracts into its install directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArchiveLayout {
    /// Extracts to a single top-level directory named exactly the version
    /// (the GE-Proton tarball shape).
    ExtractsToSingleRootDir,
}

/// What kind of install a managed runner is — drives registration and the
/// authoritative `runtime/providers.toml` inventory (ADR 0001).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InstallKind {
    /// A Proton-style compat tool (toolmanifest.vdf + `proton` script).
    CompatTool,
    /// A launcher binary placed on the executable path (`umu-run`).
    LauncherBinary,
}
