//! The umu provider (blueprint §5, §7): the managed container launch layer.
//!
//! Implements the full triple seam: `RunnerResolver` (configured path →
//! managed install → PATH for the `umu-run` binary), `ManagedRunner` (its
//! zipapp release shape), and `WrapperContributor` at `Layer::Container`,
//! contributing the `GAMEID`/`WINEPREFIX`/`PROTONPATH`/`PROTON_VERB` env
//! contract (research #18).
//!
//! Resolution and the env contract land with the managed-runner pipeline
//! (#34); until then resolve/contribute are shape-holding stubs — the umu
//! chain must not half-work inside a plain-wine plan.

use cellar_core::errors::ResolveError;
use cellar_core::manifest::{
    ArchiveLayout, ChecksumScheme, InstallKind, ReleaseSource, RunnerManifest,
};
use cellar_core::ports::{__sealed, ManagedRunner, RunnerResolver, WrapperContributor};
use cellar_core::types::{LaunchPlan, Layer, ResolvedRunner, RunnerFamily, RunnerSpec};

/// Managed umu provider.
#[derive(Debug)]
pub struct UmuProvider {
    manifest: RunnerManifest,
}

impl UmuProvider {
    /// Stable provider identifier, shared by the resolver trait and the
    /// managed-runner manifest.
    pub const ID: &'static str = "umu";

    pub fn new() -> Self {
        Self {
            manifest: RunnerManifest {
                provider_id: Self::ID.to_owned(),
                source: ReleaseSource {
                    url_template: "https://github.com/Open-Wine-Components/umu-launcher/releases/download/{tag}/umu-launcher-{tag}-zipapp.tar"
                        .to_owned(),
                    // The asset name/template is verified against release 1.4.4
                    // (2026-08); upstream publishes no checksum file for the
                    // zipapp — the verification policy lands with #34.
                    checksum_url_template: None,
                },
                checksum: ChecksumScheme::Sha512,
                archive: ArchiveLayout::ExtractsToSingleRootDir,
                install_kind: InstallKind::LauncherBinary,
            },
        }
    }
}

impl Default for UmuProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl __sealed::Sealed for UmuProvider {}

impl RunnerResolver for UmuProvider {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn resolve(&self, _spec: &RunnerSpec) -> Result<ResolvedRunner, ResolveError> {
        // Resolution (configured path → managed `umu-run` → PATH) lands
        // with the managed pipeline (#34); until then every Umu spec is
        // exhausted by construction.
        Err(ResolveError::Unresolvable {
            family: RunnerFamily::Umu,
        })
    }
}

impl ManagedRunner for UmuProvider {
    fn manifest(&self) -> &RunnerManifest {
        &self.manifest
    }
}

impl WrapperContributor for UmuProvider {
    fn layer(&self) -> Layer {
        Layer::Container
    }

    // The umu env contract (GAMEID, WINEPREFIX, PROTONPATH, PROTON_VERB) is
    // contributed by this provider — activated with the managed pipeline
    // (#34), where the plan carries a Proton install to point at.
    fn contribute(&self, _plan: &mut LaunchPlan) {}
}
