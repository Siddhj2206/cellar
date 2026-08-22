//! The Cellar launch machinery (blueprint §7, ADR 0002): launch-plan
//! resolution — the two-stage precedence (selection: app override → prefix
//! default → defaults floor; then the provider-internal resolution order
//! configured → managed → PATH) — wrapper-chain assembly sorted by `Layer`,
//! and the frozen `LaunchPlan` as a pure, printable value. A generic chain
//! builder: env contracts are wrapper-provider data, never launch machinery
//! (#21, ADR 0003).
//!
//! This slice (#28) delivers the resolve → check → plan phases with nothing
//! spawning: the selection walk and plan assembly below, the pre-flight
//! [`LaunchError`] dispositions, and an empty effective wrapper chain.
//! Spawning the frozen plan lands with the execute slice (#29).

pub mod error;
pub mod plan;

pub use error::LaunchError;
pub use plan::{apply_wrappers, build_plan, select_spec};
