//! The discover-only wine provider (blueprint §5): plain system wine found
//! via PATH — read-only, never modified by Cellar, and never a fallback for a
//! failed Proton selection (research #18).
//!
//! Implements only `RunnerResolver`; mode is trait membership, so there is
//! deliberately no `ManagedRunner` stub. The PATH lookup (execvp semantics)
//! lands with the launch slice (#28).

use cellar_core::errors::ResolveError;
use cellar_core::ports::{__sealed, RunnerResolver};
use cellar_core::types::{ResolvedRunner, RunnerSpec};

/// Discover-only wine provider.
#[derive(Debug, Default)]
pub struct WineProvider;

impl __sealed::Sealed for WineProvider {}

impl RunnerResolver for WineProvider {
    fn id(&self) -> &'static str {
        "wine"
    }

    fn resolve(&self, _spec: &RunnerSpec) -> Result<ResolvedRunner, ResolveError> {
        Err(ResolveError::Unresolvable)
    }
}
