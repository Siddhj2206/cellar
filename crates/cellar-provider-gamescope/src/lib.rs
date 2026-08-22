//! The gamescope wrapper provider (blueprint §5): a `Display`-layer wrapper
//! that prepends `gamescope` to the launch chain when configured.
//!
//! Chain prepending lands with the launch slice (#28); until then
//! [`GamescopeProvider::contribute`] is a shape-holding stub.

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
