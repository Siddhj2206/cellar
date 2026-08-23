//! The gamescope wrapper provider (blueprint §5): a `Display`-layer wrapper
//! that prepends `gamescope` to the launch chain when the bound prefix's
//! graphics default selects it (the prefix file's `graphics = "gamescope"`).
//! The binary is taken verbatim — spawn resolves it via PATH (`execvp`
//! semantics), so a distro-provided gamescope works without configuration.

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

    fn contribute(&self, plan: &mut LaunchPlan) {
        // The outermost layer of the chain: `gamescope` execs the rest.
        // Contribution runs innermost-first, so a prepend here stays
        // outer.
        plan.argv.insert(0, "gamescope".to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepends_gamescope_at_the_display_layer() {
        let mut plan = LaunchPlan {
            argv: vec!["wine".to_owned(), "game.exe".to_owned()],
            env: std::collections::BTreeMap::new(),
            cwd: None,
            wrappers: Vec::new(),
        };
        GamescopeProvider.contribute(&mut plan);
        assert_eq!(
            plan.argv,
            [
                "gamescope".to_owned(),
                "wine".to_owned(),
                "game.exe".to_owned()
            ]
        );
        assert_eq!(GamescopeProvider.layer(), Layer::Display);
    }
}
