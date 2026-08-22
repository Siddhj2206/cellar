//! The Cellar CLI — presentation, composition root (blueprint §8, ADR 0004).
//!
//! This binary is the *only* place concrete infra is instantiated: it builds
//! the provider registry and injects it into the application services.
//! Symmetric with the future `cellar-gui` leaf; flipping primary is
//! `default-members`, zero edits below presentation.
//!
//! The scaffold lands nothing user-visible: `main` proves the composition
//! root wiring and exits 0. The clap surface arrives with slice 02 (#26:
//! `cellar prefix create|list|delete`).

fn main() {
    // Composition root: presentation alone constructs concrete providers.
    let _resolvers = cellar_providers::all_resolvers();
    let _managed = cellar_providers::all_managed();
    let _wrappers = cellar_providers::all_wrappers();
}

#[cfg(test)]
mod tests {
    #[test]
    fn providers_are_wired_at_the_composition_root() {
        assert!(!cellar_providers::all_resolvers().is_empty());
        assert!(!cellar_providers::all_managed().is_empty());
        assert!(!cellar_providers::all_wrappers().is_empty());
    }
}
