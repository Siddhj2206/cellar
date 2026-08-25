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
use crate::health::{DesktopIntegration, TreeHealth};
use crate::manifest::{ManagedRecord, RunnerManifest};
use crate::types::{LaunchPlan, Layer, ResolvedRunner, RunnerSpec};

/// The phase events of the managed-install pipeline (#37): the storage
/// layer reports progress through the caller-supplied callback of
/// [`Storage::install_managed`] — it never prints. Presentation owns the
/// screen (ADR 0004): the CLI renders these as stderr progress lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallProgress {
    /// The artifact download advanced: `offset` bytes are now on disk of a
    /// `total`-byte artifact. `total` is `None` when the source names no
    /// length (a chunked response), so no percentage is derivable; ticks
    /// otherwise arrive per copied chunk, offsets monotonically rising to
    /// `total`.
    Download { offset: u64, total: Option<u64> },
    /// Checksum verification started (SHA-512 over the whole artifact).
    Verify,
    /// Extraction into the runtime directory started.
    Extract,
}

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

    /// The on-disk directory of one prefix: pure layout math, no I/O —
    /// `$root/prefixes/<slug>` (ADR 0001). The launch plan needs the path
    /// for the wine `WINEPREFIX` contract; the layout stays the adapter's
    /// knowledge, never `launch`/`app`'s.
    fn prefix_dir(&self, slug: &str) -> PathBuf;

    /// The per-launch log directory: `$root/cache/launch-logs` (blueprint
    /// §7: game output is disposable, ADR 0001). Pure layout math, no I/O —
    /// same role as [`Storage::prefix_dir`]; the launch pipeline composes
    /// `<slug>-<timestamp>.log` inside it.
    fn launch_logs_dir(&self) -> PathBuf;

    fn load_settings(&self) -> Result<Settings, StorageError>;
    fn save_settings(&self, settings: &Settings) -> Result<(), StorageError>;

    fn create_prefix(&self, slug: &str) -> Result<Prefix, StorageError>;
    fn list_prefixes(&self) -> Result<Vec<Prefix>, StorageError>;
    fn load_prefix(&self, slug: &str) -> Result<Prefix, StorageError>;
    fn save_prefix(&self, prefix: &Prefix) -> Result<(), StorageError>;
    fn delete_prefix(&self, slug: &str) -> Result<(), StorageError>;

    /// Every parsed app entry. An `apps/*.toml` that fails to parse is
    /// skipped here yet still counted by [`Storage::list_app_slugs`] —
    /// the difference between the two is exactly the hand-edit damage
    /// doctor flags and a re-derivation sweep must spare (ADR 0001).
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

    /// Discovery: the prefix's Start Menu / Desktop areas, scanned for
    /// executable candidates the user reviews. Never auto-registers. The
    /// flat `*.exe` scan of the areas landed with #30; `.lnk` target
    /// decoding — shortcuts resolved against the prefix's `drive_c` —
    /// with #31. Deterministic order, deduplicated by exe path.
    fn discover_executables(&self, prefix: &Prefix) -> Result<Vec<Candidate>, StorageError>;

    /// The shared installer pipeline — fetch (resumable), verify (SHA-512
    /// for GE-Proton), extract, flock per directory, inventory write —
    /// driven by a managed runner's manifest and a concrete version pin.
    /// The install directory (`runtime/<provider_id>/<version>`) is the
    /// return; an already-installed version is a no-op. Corrupt artifacts
    /// fail closed — nothing is extracted, nothing is recorded.
    ///
    /// Phase progress is *reported*, never printed (#37): implementations
    /// emit [`InstallProgress`] events through `progress` and leave the
    /// rendering to presentation (ADR 0004 — the screen belongs to the
    /// CLI).
    fn install_managed(
        &self,
        manifest: &RunnerManifest,
        version: &str,
        progress: &mut dyn FnMut(InstallProgress),
    ) -> Result<PathBuf, StorageError>;

    /// The authoritative managed-runner inventory (`runtime/providers.toml`
    /// — blueprint §6: the runtime directory is rebuildable from it): every
    /// record the pipeline recorded, deterministic order.
    fn managed_inventory(&self) -> Result<Vec<ManagedRecord>, StorageError>;
}

/// Desktop integration: `.desktop` entries, Rust-native icon extraction and
/// cache, MIME/file associations (blueprint §5, §8). The lifecycle is
/// strictly one-way — the adapter derives host artifacts from the
/// `AppEntry` values it is handed and never reads or writes tree state:
/// everything it produces is re-derivable from the tree (blueprint §6:
/// the cache is disposable; this slice ships the re-derivation sweep).
pub trait DesktopIntegrator: __sealed::Sealed + Send + Sync + Debug {
    /// Create or refresh the `.desktop` launcher entry for `app`. When the
    /// app was renamed — `previous_slug` differs from `app.slug` — the old
    /// entry file is removed, so the entry's file name always tracks the
    /// app's slug (blueprint §6 naming; identity stays the exe path).
    /// `icon` is the cached icon path the entry references, `None` for an
    /// icon-less entry. Returns the entry path.
    fn create_entry(
        &self,
        app: &AppEntry,
        previous_slug: Option<&str>,
        icon: Option<&Path>,
    ) -> Result<PathBuf, DesktopError>;

    /// Remove `app`'s launcher entry and its cached icon — the uninstall
    /// cleanup; a missing entry is not an error (the re-derivation sweep
    /// may already have removed it).
    fn remove_entry(&self, app: &AppEntry) -> Result<(), DesktopError>;

    /// Extract the exe's icon Rust-natively into the disposable icon
    /// cache, reusing an existing cache file; `None` when the exe carries
    /// no usable icon (entries without icons are still created — the icon
    /// key is simply omitted).
    fn install_icon(&self, exe: &Path) -> Result<Option<PathBuf>, DesktopError>;

    /// Remove this integrator's own entry files whose slugs are not in
    /// `keep` — the stale sweep of a re-derivation (a renamed or gone app
    /// never leaves an entry behind). Returns the removed paths.
    fn prune_entries(&self, keep: &[&str]) -> Result<Vec<PathBuf>, DesktopError>;

    /// Wire the "Open with Cellar" file association for Windows
    /// executables: a no-display launcher whose exec line calls the
    /// presentation binary's install entrypoint directly — no wrapper
    /// binary, no shell wrapper (ADR 0004). The declared type is exactly
    /// `application/vnd.microsoft.portable-executable`, and the
    /// implementation refreshes the host MIME index (`mimeinfo.cache`)
    /// best-effort so file managers see the association immediately
    /// (#55). Every registration calls this too — the association is
    /// never only a sync-time artifact; uninstall leaves it alone
    /// (global state).
    fn set_file_association(&self) -> Result<(), DesktopError>;

    /// The Exec target our launcher entry for `slug` records — the binary
    /// path a click would launch. `None` when no entry file exists for the
    /// slug or its Exec line is unreadable: nothing recorded to compare.
    fn entry_exec_target(&self, slug: &str) -> Result<Option<PathBuf>, DesktopError>;

    /// What this integrator records into every Exec line it writes — the
    /// comparison side of [`DesktopIntegrator::entry_exec_target`] (#57):
    /// an entry whose recorded target differs from this is repointed by
    /// the next rewrite, which is exactly the repaired count.
    fn exec_target(&self) -> &Path;

    /// The desktop-integration facts doctor's fifth section reports
    /// (#57): which owned entries point at binaries that no longer exist,
    /// and the Open-with-Cellar association's state. Staleness is
    /// existence-only — a target that exists but differs from the running
    /// binary is a legitimate multi-binary setup and is never reported;
    /// icons are never reported (the cache is disposable). Read-only:
    /// like [`Storage::tree_health`], it reports exactly what is on disk.
    fn integration_health(&self) -> Result<DesktopIntegration, DesktopError>;
}
