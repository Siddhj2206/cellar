//! The gamescope wrapper provider (blueprint §5): a `Display`-layer wrapper
//! that prepends `gamescope` to the launch chain when configured.
//!
//! Chain prepending lands when wrapper activation rules land (the managed
//! pipeline #34); until then [`GamescopeProvider::contribute`] is a
//! shape-holding stub — the launch slice's dry-run is spawn-free with an
//! empty chain.

use cellar_core::ports::{__sealed, WrapperContributor};
use cellar_core::types::{LaunchPlan, Layer};

/// Gamescope wrapper provider.
#[derive(Debug, Default)]
pub struct GamescopeProvider;

impl __sealed::Sealed for GamescopeProvider {}

impl WrapperContributor for GamescopeProvider {
    fn layer(&self) -> Layer {
        Layer::Display
    }

    fn contribute(&self, _plan: &mut LaunchPlan) {}
}
