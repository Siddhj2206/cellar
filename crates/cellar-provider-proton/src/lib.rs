//! The Proton provider (blueprint §5): managed GE-Proton / umu-Proton and
//! discover-only Steam Proton.
//!
//! Implements `RunnerResolver` (both modes) and `ManagedRunner` (a declarative
//! manifest for the storage-owned installer pipeline, research #18: SHA-512
//! verification, the `<tag>-<arch>.tar.gz` asset shape, single-root-dir
//! extraction). Discover-only Steam compat-dir scanning arrives with the
//! launch slice (#28); until then [`ProtonProvider::resolve`] answers
//! `Unresolvable`.

use cellar_core::errors::ResolveError;
use cellar_core::manifest::{
    ArchiveLayout, ChecksumScheme, InstallKind, ReleaseSource, RunnerManifest,
};
use cellar_core::ports::{__sealed, ManagedRunner, RunnerResolver};
use cellar_core::types::{ResolvedRunner, RunnerSpec};

/// Managed GE-Proton provider.
#[derive(Debug)]
pub struct ProtonProvider {
    manifest: RunnerManifest,
}

impl ProtonProvider {
    /// Stable provider identifier, shared by the resolver trait and the
    /// managed-runner manifest.
    pub const ID: &'static str = "proton";

    pub fn new() -> Self {
        Self {
            manifest: RunnerManifest {
                provider_id: Self::ID.to_owned(),
                source: ReleaseSource {
                    url_template: "https://github.com/GloriousEggroll/proton-ge-custom/releases/download/{tag}/{tag}-{arch}.tar.gz"
                        .to_owned(),
                    checksum_url_template: Some(
                        "https://github.com/GloriousEggroll/proton-ge-custom/releases/download/{tag}/{tag}-{arch}.sha512sum"
                            .to_owned(),
                    ),
                },
                checksum: ChecksumScheme::Sha512,
                archive: ArchiveLayout::ExtractsToSingleRootDir,
                install_kind: InstallKind::CompatTool,
            },
        }
    }
}

impl Default for ProtonProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl __sealed::Sealed for ProtonProvider {}

impl RunnerResolver for ProtonProvider {
    fn id(&self) -> &'static str {
        Self::ID
    }

    // Resolution (configured path → managed → Steam compat dirs) lands with
    // the launch slice (#28); the scaffold only locks the port shapes.
    fn resolve(&self, _spec: &RunnerSpec) -> Result<ResolvedRunner, ResolveError> {
        Err(ResolveError::Unresolvable)
    }
}

impl ManagedRunner for ProtonProvider {
    fn manifest(&self) -> &RunnerManifest {
        &self.manifest
    }
}
