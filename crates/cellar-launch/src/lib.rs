//! The Cellar launch machinery (blueprint §7, ADR 0002): launch-plan
//! resolution — the two-stage precedence (selection: app override → prefix
//! default → defaults floor; then the provider-internal resolution order
//! configured → managed → PATH) — wrapper-chain assembly sorted by `Layer`,
//! the frozen `LaunchPlan` as a pure, printable value, and the execute
//! phase: spawning that plan into a [`SpawnedProcess`] with per-launch
//! output under the disposable cache. A generic chain builder: env
//! contracts are wrapper-provider data, never launch machinery (#21, ADR
//! 0003).
//!
//! The phase boundary (blueprint §7) is the crate's spine: resolve → check →
//! plan are pure and spawn-free — `--dry-run` and the GUI preview render
//! them free (slice #28); the first `exec` happens in [`execute::spawn`],
//! and the wait-vs-detach policy stays with the presentation (slice #29).

pub mod error;
pub mod execute;
pub mod plan;

pub use error::LaunchError;
pub use execute::{LaunchMode, SpawnedProcess, spawn};
pub use plan::{apply_wrappers, build_plan, select_spec};
