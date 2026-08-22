//! The provider registry (blueprint §4): static, explicit wiring of the
//! provider crates, consumed by presentation crates only (composition root —
//! nothing below presentation constructs infra). The registry API is the
//! three `all_*` functions below (research #17); presentations bind their
//! results at the composition root.
//!
//! Adding a provider = one new crate + one line here + one presentation dep —
//! zero edits below presentation (research #17: explicit registry over
//! link-time `inventory` magic).
//!
//! The blueprint's "provider crates only" line means the registry never
//! depends on `app`/`launch`/`storage`/`desktop`; it still depends on
//! `cellar-core`, whose trait types (`dyn RunnerResolver` & co.) are the
//! registry's return vocabulary (ADR 0002 amendment, 2026-08).

use cellar_core::ports::{ManagedRunner, RunnerResolver, WrapperContributor};
use cellar_provider_gamescope::GamescopeProvider;
use cellar_provider_proton::ProtonProvider;
use cellar_provider_umu::UmuProvider;
use cellar_provider_wine::WineProvider;

/// Every runner-resolving provider in the workspace.
pub fn all_resolvers() -> Vec<Box<dyn RunnerResolver>> {
    vec![
        Box::new(ProtonProvider::new()),
        Box::new(UmuProvider::new()),
        Box::new(WineProvider),
    ]
}

/// Every managed provider (drives the storage-owned installer pipeline).
pub fn all_managed() -> Vec<Box<dyn ManagedRunner>> {
    vec![
        Box::new(ProtonProvider::new()),
        Box::new(UmuProvider::new()),
    ]
}

/// Every wrapper contributor, in no particular order (launch sorts by
/// `Layer`).
pub fn all_wrappers() -> Vec<Box<dyn WrapperContributor>> {
    vec![Box::new(GamescopeProvider), Box::new(UmuProvider::new())]
}

#[cfg(test)]
mod tests {
    use cellar_core::types::Layer;

    use super::*;

    #[test]
    fn registry_wires_every_provider() {
        let resolver_ids: Vec<_> = all_resolvers().iter().map(|r| r.id()).collect();
        assert_eq!(resolver_ids, ["proton", "umu", "wine"]);
        assert_eq!(all_managed().len(), 2);
        assert_eq!(all_wrappers().len(), 2);
    }

    #[test]
    fn wrapper_chain_is_layer_sortable_from_the_registry() {
        let mut layers: Vec<_> = all_wrappers().iter().map(|w| w.layer()).collect();
        layers.sort();
        assert_eq!(layers, [Layer::Display, Layer::Container]);
    }
}
