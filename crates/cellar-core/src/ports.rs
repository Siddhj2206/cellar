//! The five sealed extension ports (blueprint §5, ADR 0003).
//!
//! Exactly five traits — `RunnerResolver`, `ManagedRunner`,
//! `WrapperContributor`, `Storage`, `DesktopIntegrator` — all `Send + Sync`,
//! object-safe, and sealed. `app`/`launch` use generics over them; the
//! composition root (presentation crates only) uses `Box<dyn _>`.
//!
//! Sealing is the workspace variant of the pattern: the marker trait lives in
//! the doc-hidden [`__sealed`] module, so only crates that deliberately opt
//! in — the provider crates and the storage/desktop adapters inside this
//! workspace — can implement a port. Adding a provider never touches
//! `core`/`app`/`launch`.

use std::fmt::Debug;
use std::path::{Path, PathBuf};

use crate::entities::{AppEntry, Candidate, Prefix, Settings};
use crate::errors::{DesktopError, ResolveError, StorageError};
use crate::health::TreeHealth;
use crate::manifest::RunnerManifest;
use crate::types::{LaunchPlan, Layer, ResolvedRunner, RunnerSpec};

/// Marker that seals the port traits against implementations outside the
/// Cellar workspace. Doc-hidden on purpose: the surface is closed to the
/// outside world, open to this workspace's crates.
#[doc(hidden)]
pub mod __sealed {
    pub trait Sealed {}
}

/// Resolves runner specs to concrete runner references, in both modes.
/// Managed and discover-only providers implement this trait (glossary:
/// Provider, `RunnerRef`).
pub trait RunnerResolver: __sealed::Sealed + Send + Sync + Debug {
    /// Stable identifier, e.g. "proton", "wine", "umu".
    fn id(&self) -> &'static str;

    /// Resolve a spec via this family's order: configured path → managed
    /// install → PATH (research #18).
    fn resolve(&self, spec: &RunnerSpec) -> Result<ResolvedRunner, ResolveError>;
}

/// Managed-only lifecycle: a declarative [`RunnerManifest`] describing how
/// this runner is acquired and installed. Discover-only providers never
/// implement this trait — mode is trait membership (no stubs).
pub trait ManagedRunner: __sealed::Sealed + Send + Sync + Debug {
    fn manifest(&self) -> &RunnerManifest;
}

/// Layers environment and behavior onto a launch plan at a declared [`Layer`].
/// Env contracts are wrapper-provider data — launch machinery never
/// hard-codes them (research #18, ADR 0003).
pub trait WrapperContributor: __sealed::Sealed + Send + Sync + Debug {
    fn layer(&self) -> Layer;

    /// Mutate the plan's argv/env in place; called in `Layer` order.
    fn contribute(&self, plan: &mut LaunchPlan);
}

/// The file tree: CRUD, `.lnk` discovery, the shared installer pipeline, and
/// XDG path resolution — one external system, one port; the sole writer of
/// the tree (ADR 0001).
pub trait Storage: __sealed::Sealed + Send + Sync + Debug {
    /// The tree root: `$XDG_DATA_HOME/cellar`.
    fn data_root(&self) -> &Path;

    fn load_settings(&self) -> Result<Settings, StorageError>;
    fn save_settings(&self, settings: &Settings) -> Result<(), StorageError>;

    fn create_prefix(&self, slug: &str) -> Result<Prefix, StorageError>;
    fn list_prefixes(&self) -> Result<Vec<Prefix>, StorageError>;
    fn load_prefix(&self, slug: &str) -> Result<Prefix, StorageError>;
    fn save_prefix(&self, prefix: &Prefix) -> Result<(), StorageError>;
    fn delete_prefix(&self, slug: &str) -> Result<(), StorageError>;

    fn list_apps(&self) -> Result<Vec<AppEntry>, StorageError>;
    fn load_app(&self, slug: &str) -> Result<AppEntry, StorageError>;
    fn save_app(&self, app: &AppEntry) -> Result<(), StorageError>;
    fn delete_app(&self, slug: &str) -> Result<(), StorageError>;

    /// The app-slug dedupe domain: every `apps/*.toml` file stem, valid or
    /// not. A fresh install dedupes against this set so a broken hand-edited
    /// entry is never overwritten (ADR 0001).
    fn list_app_slugs(&self) -> Result<Vec<String>, StorageError>;

    /// The canonical absolute path of an executable — the `AppEntry`
    /// identity (blueprint §6: "identity stays the canonical exe path").
    /// Validates the path resolves and is a file — the registration-time
    /// pre-flight (a registered entry always points at a real file; an exe
    /// deleted later is flagged by `tree_health`).
    fn canonicalize_exe(&self, path: &Path) -> Result<PathBuf, StorageError>;

    /// Health of the tree (blueprint §7: the doctor applied tree-wide):
    /// which nodes are missing, which files are invalid, which prefix dirs
    /// are orphaned. Deliberately no side effects — it reports exactly what
    /// is on disk, never initializes anything.
    fn tree_health(&self) -> Result<TreeHealth, StorageError>;

    /// Discovery: read the prefix's Start Menu / Desktop `.lnk` files and
    /// list candidate executables for user review. Never auto-registers.
    fn discover_executables(&self, prefix: &Prefix) -> Result<Vec<Candidate>, StorageError>;

    /// The shared installer pipeline — fetch, verify, extract, flock,
    /// resumable cache, inventory write — driven by a managed runner's
    /// manifest. Returns the install directory.
    fn install_managed(&self, manifest: &RunnerManifest) -> Result<PathBuf, StorageError>;
}

/// Desktop integration: `.desktop` entries, Rust-native icon extraction and
/// cache, MIME/file associations (blueprint §8).
pub trait DesktopIntegrator: __sealed::Sealed + Send + Sync + Debug {
    /// Create or refresh the `.desktop` launcher entry. Returns the entry
    /// path.
    fn create_entry(&self, app: &AppEntry) -> Result<PathBuf, DesktopError>;
    fn remove_entry(&self, app: &AppEntry) -> Result<(), DesktopError>;
    /// Extract an icon from the exe into the icon cache; returns the cache
    /// path.
    fn install_icon(&self, app: &AppEntry, exe: &Path) -> Result<PathBuf, DesktopError>;
    /// Wire the "Open with Cellar" MIME association.
    fn set_file_association(&self, mime_type: &str) -> Result<(), DesktopError>;
}
