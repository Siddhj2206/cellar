//! # Cellar core
//!
//! The domain layer of the Cellar workspace: pure types, the five sealed
//! extension ports, and invariants — **no I/O, no platform** (blueprint §4).
//! Dependency budget: std + serde only.
//!
//! Everything else in the workspace builds on this crate:
//!
//! - [`types`] — runner families, specs and refs, the `Layer` order, the
//!   printable `LaunchPlan`.
//! - [`manifest`] — declarative managed-runner descriptors.
//! - [`entities`] — `Prefix`, `AppEntry`, `Settings`, `Candidate` (glossary
//!   vocabulary, CONTEXT.md).
//! - [`slug`] — tree file-name rules: slugify, validation, `-2` dedupe.
//! - [`health`] — the `TreeHealth` report the doctor renders.
//! - [`ports`] — the locked extension seam (ADR 0003): exactly five traits.
//! - [`errors`] — pre-flight error families for the ports.

pub mod entities;
pub mod errors;
pub mod health;
pub mod manifest;
pub mod ports;
pub mod slug;
pub mod types;

pub use entities::{AppEntry, AppKind, Candidate, Overrides, Prefix, PrefixDefaults, Settings};
pub use errors::{DesktopError, ResolveError, StorageError};
pub use health::TreeHealth;
pub use manifest::{ArchiveLayout, ChecksumScheme, InstallKind, ReleaseSource, RunnerManifest};
pub use slug::{MAX_LEN, dedupe_slug, is_valid_slug, slugify};
pub use types::{
    ConfiguredRunner, LaunchPlan, Layer, ProviderMode, ResolvedRunner, RunnerFamily, RunnerInstall,
    RunnerRef, RunnerSpec,
};

#[cfg(test)]
mod tests {
    use super::entities::Settings;
    use super::types::{Layer, RunnerFamily, RunnerSpec};

    #[test]
    fn layer_chain_order_is_display_container_runtime_env() {
        let mut chain = vec![Layer::RuntimeEnv, Layer::Display, Layer::Container];
        chain.sort();
        assert_eq!(chain, [Layer::Display, Layer::Container, Layer::RuntimeEnv]);
    }

    #[test]
    fn plain_runner_spec_has_no_configuration() {
        let spec = RunnerSpec::new(RunnerFamily::Wine);
        assert_eq!(spec.family, RunnerFamily::Wine);
        assert!(spec.configured.is_none());
    }

    #[test]
    fn default_settings_declare_no_resolution_order() {
        // Wine is never an automatic fallback (blueprint §7): the empty
        // default defers to kind presets and config, which land with #26/#28.
        let settings = Settings::default();
        assert!(settings.resolution_order.is_empty());
    }

    #[test]
    fn ports_are_object_safe_and_send_sync() {
        // Compile-time proof of the blueprint §5 seam contract: every port is
        // object-safe (trait objects form) and `Send + Sync` (the objects
        // meet the bound). Each line fails to compile otherwise.
        fn assert_send_sync<T: Send + Sync>() {
            let _ = std::marker::PhantomData::<T>;
        }

        assert_send_sync::<Box<dyn super::ports::RunnerResolver>>();
        assert_send_sync::<Box<dyn super::ports::ManagedRunner>>();
        assert_send_sync::<Box<dyn super::ports::WrapperContributor>>();
        assert_send_sync::<Box<dyn super::ports::Storage>>();
        assert_send_sync::<Box<dyn super::ports::DesktopIntegrator>>();
    }
}
