//! The future GUI presentation leaf (blueprint §4): symmetric with the CLI —
//! the same application services, the same provider registry, zero edits
//! below presentation.
//!
//! Scaffold-only: this placeholder compile-checks the leaf's dependency
//! edges (`app`, `core` DTOs, `providers`) so the presentation seam stays
//! green and symmetric. The real widget surface lands when the GUI slice is
//! taken.

/// Future GUI entry point. Wire the same application services the CLI wires;
/// presentation alone instantiates concrete providers (composition root).
pub fn entrypoint() {
    // Composition root, like the CLI: presentation alone instantiates
    // concrete providers.
    let _resolvers = cellar_providers::all_resolvers();
    let _managed = cellar_providers::all_managed();
    let _wrappers = cellar_providers::all_wrappers();
}
