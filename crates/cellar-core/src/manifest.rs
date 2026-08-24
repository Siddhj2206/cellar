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
    /// The provider's "newest release" URL — the releases-latest page
    /// (`https://github.com/<owner>/<repo>/releases/latest`) that
    /// redirects to the newest release's page whose path ends in the
    /// tag (#65). Explicit by design: never derived or guessed from
    /// `url_template`; absent means the provider offers no latest
    /// resolution — installing `latest` there is a loud refusal, not a
    /// guess. Chosen over the GitHub REST API deliberately: the
    /// unauthenticated API caps at 60 requests/hour per IP (shared
    /// networks exhaust it instantly), while the releases page carries
    /// no such budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_url: Option<String>,
}

/// The version sentinel that resolves through the provider's release feed
/// at install time (`ReleaseSource::latest_url`) — also what an omitted
/// version pin means on the CLI surface (#65). Resolution happens once,
/// inside the installer pipeline; everything downstream (probe,
/// inventory, list) sees only the concrete tag it resolved to.
pub const LATEST_PIN: &str = "latest";

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

/// One authoritative inventory record: a managed install the pipeline
/// recorded in `runtime/providers.toml` (blueprint §6 — authoritative,
/// re-installable). Enough to rebuild the install: the provider manifest
/// (looked up by `provider_id`) plus the version pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedRecord {
    /// Stable provider identifier, matching [`RunnerManifest::provider_id`].
    pub provider_id: String,
    /// The installed version (the release tag, e.g. `GE-Proton11-5`).
    pub version: String,
    /// The install directory, relative to the runtime root
    /// (`<provider_id>/<version>`) — the tree is one movable unit (ADR
    /// 0001), so records never carry absolute paths.
    pub install: String,
}

/// The inventory file's entity (blueprint §6): one `[[runner]]` table per
/// record — the wrapper keeps the file a single TOML table (the envelope
/// already carries `schema_version`; a bare top-level array is not a TOML
/// shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ManagedInventory {
    #[serde(default)]
    pub runner: Vec<ManagedRecord>,
}
