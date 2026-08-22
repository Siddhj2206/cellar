//! Application use-cases: the `AppEntry` registry — install with the
//! three-armed artifact handling (standalone registers without executing,
//! installer runs inside the prefix with its exit awaited, archive extracts
//! into it — each followed by the flat discovery scan, #30) — app list with
//! per-entry status, uninstall degrading to entry removal (#27), the prefix
//! lifecycle and the tree-health doctor check (#26), and the `LaunchApp`
//! use-case (#28/#29): resolve → check → plan → execute. Thin orchestration
//! over the `core` ports — concrete adapters are injected only at the
//! composition root.

use cellar_core::Prefix;
use cellar_core::entities::{AppEntry, AppKind, Candidate, Overrides};
use cellar_core::errors::StorageError;
use cellar_core::health::TreeHealth;
use cellar_core::ports::{RunnerResolver, Storage};
use cellar_core::slug;
use cellar_core::types::{LaunchPlan, RunnerSpec};
use cellar_launch::{LaunchError, LaunchMode, SpawnedProcess, build_plan, select_spec};

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::str::FromStr;

use crate::archive::{ArchiveError, extract_zip};

/// The check-phase status of a registered entry (blueprint §7: the check
/// phase applied entry-wide). This slice checks the registered exe's
/// presence; the runner-integrity and wrapper checks land with the launch
/// slice (#28).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryStatus {
    Ok,
    /// The registered exe is missing from disk (deleted or moved) — the
    /// blueprint §7 disposition: re-register or uninstall.
    ExeMissing,
}

impl EntryStatus {
    /// The label for tables and JSON (stable output vocabulary).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::ExeMissing => "missing-exe",
        }
    }
}

/// One row of `cellar list`: a registered entry plus its current status and
/// the runner default of the prefix it binds to — the rung that joins the
/// runner column's chain (pinned ref → app override → prefix default →
/// kind preset) with the launch slice (#28).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedEntry {
    pub entry: AppEntry,
    pub status: EntryStatus,
    /// The bound prefix's default runner spec (the binding override picks
    /// which prefix's defaults apply), when that prefix is readable. A
    /// missing or broken prefix renders the floor instead — the doctor
    /// flags such trees.
    pub prefix_runner: Option<RunnerSpec>,
}

/// The result of a registration that happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    pub entry: AppEntry,
    /// Whether the registration updated an existing entry with the same exe
    /// (identity = canonical exe path) instead of creating a new one.
    pub was_update: bool,
}

/// The result of one session (glossary: InstallSession): the prefix it
/// touched, the entries registered — this slice: standalone only; candidate
/// review and multi-registration land with #31 — and, for the running and
/// extracting branches, the flat discovery scan of the prefix's menu and
/// desktop areas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    /// The prefix the session touched — the bound one, created when missing.
    pub prefix_slug: String,
    pub registrations: Vec<InstallResult>,
    /// Executable candidates found after the artifact ran or extracted
    /// (flat scan, `.lnk` decoding lands with #31). Empty for standalone.
    pub candidates: Vec<Candidate>,
    /// The artifact-run log under the disposable cache (installer branch —
    /// the run's output always lands in a per-launch log, blueprint §7).
    pub log_path: Option<PathBuf>,
}

/// The artifact branch of `cellar install` (blueprint §8 step 2): how the
/// artifact is handled inside the session. The flag replaces the interactive
/// question with its filename-hint default, which lands with the discovery
/// slice (#31) — the branch is never guessed silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// Register the exe without executing anything (the #27 branch).
    Standalone,
    /// Run the installer inside the bound prefix, awaiting its exit.
    Installer,
    /// Extract the archive into the bound prefix.
    Archive,
}

impl FromStr for ArtifactKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "standalone" => Ok(Self::Standalone),
            "installer" => Ok(Self::Installer),
            "archive" => Ok(Self::Archive),
            other => Err(format!(
                "expected standalone, installer, or archive, got {other:?}"
            )),
        }
    }
}

/// Session failures of `cellar install` (blueprint §8 step 2): the storage
/// and launch taxonomies passed through, the installer's own exit, and
/// archive extraction — each in its own vocabulary, with the fix where the
/// pipeline defines one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    Storage(StorageError),
    /// The installer's plan could not be built, spawned, or captured — the
    /// launch pipeline's taxonomy (the installer running but failing is
    /// [`InstallError::InstallerFailed`]).
    Launch(LaunchError),
    /// The installer ran but failed: the session aborts, nothing is
    /// registered. The exit facts are reported raw (Runtime family,
    /// blueprint §7) — no magic mapping.
    InstallerFailed {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Archive(ArchiveError),
}

impl From<StorageError> for InstallError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<LaunchError> for InstallError {
    fn from(error: LaunchError) -> Self {
        Self::Launch(error)
    }
}

impl From<ArchiveError> for InstallError {
    fn from(error: ArchiveError) -> Self {
        Self::Archive(error)
    }
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => write!(f, "{error}"),
            Self::Launch(error) => write!(f, "{error}"),
            Self::InstallerFailed { code, signal } => match (code, signal) {
                (Some(code), _) => write!(
                    f,
                    "the installer failed: exited with code {code} — the session aborted"
                ),
                (None, Some(signal)) => write!(
                    f,
                    "the installer failed: terminated by signal {signal} — the session aborted"
                ),
                (None, None) => write!(f, "the installer failed — the session aborted"),
            },
            Self::Archive(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for InstallError {}

/// The `AppEntry` registry and install session (blueprint §8: `cellar
/// install <path>` is the flagship flow). Identity is the canonical exe
/// path — re-installing the same exe updates the same entry (blueprint §6,
/// §8); the slug is the display/file name with `-2` dedupe. This slice
/// (#30) delivers the closed three-armed artifact handling (blueprint §5):
/// standalone, installer (run inside the bound prefix, exit awaited),
/// archive (extract into it) — the running/extracting branches then
/// collect the prefix's menu/desktop candidates for review.
pub struct InstallService<S: Storage, R: RunnerResolver> {
    storage: S,
    resolver: R,
}

impl<S: Storage, R: RunnerResolver> InstallService<S, R> {
    /// The service over one storage adapter and one resolver — the
    /// composition root injects the concrete registry composite (#28).
    pub fn new(storage: S, resolver: R) -> Self {
        Self { storage, resolver }
    }

    /// The flagship flow's artifact handling (blueprint §8 steps 1–3 for
    /// now; the interactive review and summary complete the flow with #31):
    /// bind or create the prefix, then handle the artifact per its
    /// [`ArtifactKind`]. Standalone registers without executing (the #27
    /// branch, unchanged); installer runs inside the prefix with its exit
    /// awaited — a failed installer aborts the session with
    /// [`InstallError::InstallerFailed`]; archive extracts into the
    /// prefix's wine root, path-traversal-safe. The running/extracting
    /// branches then collect the prefix's menu/desktop executable
    /// candidates for review (flat scan; `.lnk` decoding lands with #31).
    pub fn install(
        &self,
        path: &Path,
        prefix: &str,
        name: Option<&str>,
        kind: AppKind,
        artifact: ArtifactKind,
    ) -> Result<InstallOutcome, InstallError> {
        if !slug::is_valid_slug(prefix) {
            return Err(StorageError::Invalid(format!("invalid prefix slug {prefix:?}")).into());
        }
        // The file-exists preflight every branch shares; the shape check is
        // branch-specific (an archive may be any regular file — the content
        // decides, `NotAnArchive`).
        let canonical = self.storage.canonicalize_exe(path)?;
        if artifact != ArtifactKind::Archive && !is_exe(&canonical) {
            return Err(StorageError::Invalid(format!(
                "{}: not a Windows executable — only .exe files are installed \
                 standalone or as installers",
                canonical.display()
            ))
            .into());
        }
        // The standalone branch's naming is decided up front — an
        // unslugifiable display name is rejected before any side effect
        // (the #27 behavior, unchanged).
        let standalone_base = if artifact == ArtifactKind::Standalone {
            let display_name = match name {
                Some(name) => name.to_owned(),
                None => canonical
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            };
            let base = slug::slugify(&display_name);
            if base.is_empty() {
                return Err(StorageError::Invalid(format!(
                    "cannot form an app slug from {display_name:?}"
                ))
                .into());
            }
            Some(base)
        } else {
            None
        };
        // Pick or create the bound prefix (blueprint §8 step 1, default
        // `default`). A missing or broken hand-edited prefix yields a fresh
        // sibling via the storage dedupe — never a clobber (ADR 0001).
        let bound = match self.storage.load_prefix(prefix) {
            Ok(existing) => existing,
            Err(StorageError::NotFound(_) | StorageError::Invalid(_)) => {
                self.storage.create_prefix(prefix)?
            }
            Err(other) => return Err(InstallError::Storage(other)),
        };
        let prefix_slug = bound.slug.clone();
        match artifact {
            ArtifactKind::Standalone => {
                let base = standalone_base
                    .as_deref()
                    .expect("the standalone base was computed above");
                let result = self.register_standalone(&canonical, &prefix_slug, kind, base)?;
                Ok(InstallOutcome {
                    prefix_slug,
                    registrations: vec![result],
                    candidates: Vec::new(),
                    log_path: None,
                })
            }
            ArtifactKind::Installer => {
                let (log_path, candidates) = self.run_installer(&canonical, &bound)?;
                Ok(InstallOutcome {
                    prefix_slug,
                    registrations: Vec::new(),
                    candidates,
                    log_path: Some(log_path),
                })
            }
            ArtifactKind::Archive => {
                let dest = self.storage.prefix_dir(&prefix_slug).join("drive_c");
                extract_zip(&canonical, &dest)?;
                let candidates = self
                    .storage
                    .discover_executables(&bound)
                    .map_err(InstallError::Storage)?;
                Ok(InstallOutcome {
                    prefix_slug,
                    registrations: Vec::new(),
                    candidates,
                    log_path: None,
                })
            }
        }
    }

    /// The standalone branch (blueprint §8: register without executing) —
    /// the #27 logic, unchanged. Re-installing the same exe updates the
    /// same entry — its slug and identity stay, its kind and prefix binding
    /// take the new flags. The display name was already judged usable by
    /// the session preflight (no side effects on a bad name).
    fn register_standalone(
        &self,
        canonical: &Path,
        prefix_slug: &str,
        kind: AppKind,
        base: &str,
    ) -> Result<InstallResult, StorageError> {
        // Identity is the canonical exe path: re-install finds the existing
        // entry and updates it in place.
        if let Some(mut existing) = self
            .storage
            .list_apps()?
            .into_iter()
            .find(|app| app.exe == canonical)
        {
            existing.kind = kind;
            prefix_slug.clone_into(&mut existing.prefix);
            self.storage.save_app(&existing)?;
            return Ok(InstallResult {
                entry: existing,
                was_update: true,
            });
        }
        // The dedupe domain is every app file stem — a broken hand-edited
        // entry is sidestepped, never overwritten (ADR 0001).
        let taken: BTreeSet<String> = self.storage.list_app_slugs()?.into_iter().collect();
        let app = AppEntry {
            slug: slug::dedupe_slug(base, &taken),
            exe: canonical.to_path_buf(),
            kind,
            prefix: prefix_slug.to_owned(),
            overrides: Overrides::default(),
            runner: None,
            source_installer: None,
            installed_at: None,
        };
        self.storage.save_app(&app)?;
        Ok(InstallResult {
            entry: app,
            was_update: false,
        })
    }

    /// The installer branch (blueprint §8 step 2: "installer: run inside
    /// the prefix, exit awaited", §7: "`InstallSession` always awaits the
    /// artifact's exit"). The installer runs through the launch pipeline as
    /// a tool artifact — the prefix's runner default, or the tool floor
    /// (wine) on a prefix without one; the app's own kind applies at
    /// registration (#31). A non-zero exit aborts the session; a clean run
    /// moves on to discovery.
    fn run_installer(
        &self,
        canonical: &Path,
        bound: &Prefix,
    ) -> Result<(PathBuf, Vec<Candidate>), InstallError> {
        let stem = canonical
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let slug = slug::slugify(&stem);
        if slug.is_empty() {
            return Err(StorageError::Invalid(format!(
                "cannot form a slug from the installer name {stem:?}"
            ))
            .into());
        }
        // A synthetic entry: no overrides, kind Tool — the artifact run is
        // a tool operation, so the defaults floor is wine unless the prefix
        // pins a runner.
        let entry = AppEntry {
            slug: slug.clone(),
            exe: canonical.to_path_buf(),
            kind: AppKind::Tool,
            prefix: bound.slug.clone(),
            overrides: Overrides::default(),
            runner: None,
            source_installer: None,
            installed_at: None,
        };
        let plan = plan_for(&self.storage, &self.resolver, &entry, bound, &[])?;
        let log_path = launch_log_path(&self.storage, &slug);
        let process = cellar_launch::spawn(&plan, &log_path, LaunchMode::Foreground)?;
        let status = process.wait()?;
        let (code, signal) = exit_info(status);
        if code != Some(0) {
            return Err(InstallError::InstallerFailed { code, signal });
        }
        let candidates = self
            .storage
            .discover_executables(bound)
            .map_err(InstallError::Storage)?;
        Ok((log_path, candidates))
    }

    /// Every registered entry with its status, the `cellar list` data
    /// (blueprint §8: slug, kind, prefix, runner, status). The runner
    /// column's chain includes the bound prefix's default (the rung #27
    /// deferred to the launch slice); invalid hand-edited entries are
    /// skipped — the doctor flags them (ADR 0001).
    pub fn list(&self) -> Result<Vec<ListedEntry>, StorageError> {
        let entries = self.storage.list_apps()?;
        let missing: BTreeSet<String> = self
            .storage
            .tree_health()?
            .missing_exes
            .into_iter()
            .collect();
        let mut listed = Vec::with_capacity(entries.len());
        for entry in entries {
            // The prefix-binding override decides which prefix's defaults
            // apply (glossary: Override); a broken prefix degrades the
            // column to the floor — list must still render, the doctor
            // flags the tree.
            let bound = entry.overrides.prefix.as_deref().unwrap_or(&entry.prefix);
            let prefix_runner = match self.storage.load_prefix(bound) {
                Ok(prefix) => prefix.defaults.runner,
                Err(_) => None,
            };
            listed.push(ListedEntry {
                status: if missing.contains(&entry.slug) {
                    EntryStatus::ExeMissing
                } else {
                    EntryStatus::Ok
                },
                entry,
                prefix_runner,
            });
        }
        Ok(listed)
    }

    /// Remove exactly that entry's state — its app file, never more
    /// (ADR 0001 ownership). The Windows-uninstaller path (glossary:
    /// Uninstall) degrades to entry removal for now (#30); Cellar never
    /// deletes the app's own files.
    pub fn uninstall(&self, slug: &str) -> Result<(), StorageError> {
        if !slug::is_valid_slug(slug) {
            return Err(StorageError::Invalid(format!("invalid app slug {slug:?}")));
        }
        self.storage.delete_app(slug)
    }
}

/// The frozen plan for one entry over already-loaded state — the pipeline
/// tail (selection → resolution → check → plan, blueprint §7) shared by
/// registered launches (#28) and the install session's artifact runs (#30).
fn plan_for<S: Storage, R: RunnerResolver>(
    storage: &S,
    resolver: &R,
    entry: &AppEntry,
    prefix: &Prefix,
    args: &[String],
) -> Result<LaunchPlan, LaunchError> {
    let settings = storage.load_settings().map_err(LaunchError::Storage)?;
    // Selection: app override → prefix default → defaults floor picks the
    // spec; the resolver runs the family's order (configured → managed →
    // PATH, research #18).
    let spec = select_spec(entry, prefix, &settings);
    let runner = resolver.resolve(&spec).map_err(LaunchError::Resolve)?;
    // The check stage: exactly this launch's dependencies — the exe must
    // still be a regular file.
    storage
        .canonicalize_exe(&entry.exe)
        .map_err(|err| match err {
            StorageError::NotFound(_) | StorageError::Invalid(_) => LaunchError::ExeMissing {
                slug: entry.slug.clone(),
                exe: entry.exe.clone(),
            },
            other => LaunchError::Storage(other),
        })?;
    // The plan stage: the pure, printable plan. The wrapper chain is empty
    // this slice (no wrapper activation rules yet, #34).
    let prefix_dir = storage.prefix_dir(&prefix.slug);
    build_plan(entry, prefix, &runner, &prefix_dir, &[], args)
}

/// The per-launch log file: `<slug>-<timestamp>.log` under the disposable
/// cache (blueprint §7 naming; the directory is the adapter's layout via
/// [`Storage::launch_logs_dir`]). The timestamp is epoch nanoseconds from
/// the system clock (with a defensive 0 fallback) — practically unique per
/// launch, so two launches of the same app never fight over one log; the
/// open is append-only, never truncate, as a second guard. Shared by
/// launches and the install session's artifact runs (#30).
fn launch_log_path<S: Storage>(storage: &S, slug: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    storage
        .launch_logs_dir()
        .join(format!("{slug}-{nanos}.log"))
}

/// The exit facts of a run: the code plus, on Unix, the terminating signal
/// — the Runtime-family payload the session reports raw (blueprint §7).
fn exit_info(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        (status.code(), status.signal())
    }
    #[cfg(not(unix))]
    {
        (status.code(), None)
    }
}

/// The `LaunchApp` use-case (blueprint §7): resolve → check → plan →
/// execute, with nothing spawning before the plan is frozen. This slice
/// (#28) delivered the frozen plan as a pure, printable value — `--dry-run`
/// and the GUI preview render it; the execute phase lands here as well:
/// [`LaunchApp::spawn`] runs the plan and returns a [`SpawnedProcess`]
/// handle whose per-launch log lives under the disposable cache, and the
/// presentation decides the wait-vs-detach policy (§7).
///
/// The check phase is doctor applied to one launch, in taxonomy order: the
/// bound prefix must exist (disposition: recreate), runner resolution must
/// succeed (dispositions: `SuggestInstall` / reinstall, in the
/// `ResolveError` messages), and the registered exe must still be a file
/// (disposition: re-register). First failure wins — the pipeline stops
/// behaving non-deterministically.
///
/// The wrapper contributors this slice wires are none — the effective chain
/// of every plan is empty until the umu/gamescope activation rules land
/// (#34); the chain machinery is real and tested in `cellar-launch`.
pub struct LaunchApp<S: Storage, R: RunnerResolver> {
    storage: S,
    resolver: R,
}

impl<S: Storage, R: RunnerResolver> LaunchApp<S, R> {
    /// The use-case over one storage adapter and one resolver — the
    /// composition root injects the concrete registry composite (#28).
    pub fn new(storage: S, resolver: R) -> Self {
        Self { storage, resolver }
    }

    /// The frozen plan for one registered app, or the first pre-flight
    /// failure (taxonomy order, blueprint §7). The launch never writes the
    /// tree and never spawns — dry-run is a free, faithful preview.
    pub fn plan(&self, slug: &str, args: &[String]) -> Result<LaunchPlan, LaunchError> {
        // Resolve stage, first step: the app itself.
        let entry = self.storage.load_app(slug).map_err(|err| match err {
            StorageError::NotFound(_) => LaunchError::AppNotFound {
                slug: slug.to_owned(),
            },
            other => LaunchError::Storage(other),
        })?;
        // Resolve stage, selection: the prefix-binding override decides
        // which prefix's defaults apply (glossary: Override); its default
        // is the prefix that registered the entry.
        let prefix_slug = entry
            .overrides
            .prefix
            .clone()
            .unwrap_or_else(|| entry.prefix.clone());
        let prefix = self
            .storage
            .load_prefix(&prefix_slug)
            .map_err(|err| match err {
                StorageError::NotFound(_) | StorageError::Invalid(_) => {
                    LaunchError::PrefixMissing {
                        slug: prefix_slug.clone(),
                    }
                }
                other => LaunchError::Storage(other),
            })?;
        // The shared pipeline tail: selection → resolution → check → plan.
        plan_for(&self.storage, &self.resolver, &entry, &prefix, args)
    }

    /// The execute phase (blueprint §7): freeze the plan exactly as
    /// [`LaunchApp::plan`] would, then spawn it into a
    /// [`SpawnedProcess`]. Output always goes to a fresh
    /// `<slug>-<timestamp>.log` inside the tree's disposable
    /// `cache/launch-logs` (blueprint §7 naming; the directory is the
    /// adapter's layout via [`Storage::launch_logs_dir`]). Nothing spawns
    /// before the plan is frozen — `--dry-run` stays spawn-free.
    pub fn spawn(
        &self,
        slug: &str,
        args: &[String],
        mode: LaunchMode,
    ) -> Result<SpawnedProcess, LaunchError> {
        let plan = self.plan(slug, args)?;
        let log_path = launch_log_path(&self.storage, slug);
        cellar_launch::spawn(&plan, &log_path, mode)
    }
}

/// Whether a canonical path names a Windows executable (case-insensitive
/// `.exe`, per Windows file naming).
fn is_exe(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
}

/// Prefix lifecycle: create with slug naming and `-2` dedupe, list, delete
/// with directory ownership (blueprint §6, ADR 0001).
pub struct PrefixService<S: Storage> {
    storage: S,
}

impl<S: Storage> PrefixService<S> {
    /// The service over one storage adapter.
    pub fn new(storage: S) -> Self {
        Self { storage }
    }

    /// Create a prefix: the name is slugified (blueprint §6 naming), and the
    /// resulting slug is deduped against every existing prefix directory —
    /// including broken ones, so a hand-edited entry is never clobbered.
    /// Returns the created prefix with its final (deduped) slug.
    pub fn create(&self, name: &str) -> Result<Prefix, StorageError> {
        let slug = slug::slugify(name);
        if slug.is_empty() {
            return Err(StorageError::Invalid(format!(
                "cannot form a prefix slug from {name:?}"
            )));
        }
        self.storage.create_prefix(&slug)
    }

    /// Every valid prefix in the tree. Invalid hand-edited entries are
    /// skipped here — the doctor flags them (ADR 0001).
    pub fn list(&self) -> Result<Vec<Prefix>, StorageError> {
        self.storage.list_prefixes()
    }

    /// Delete a prefix and exactly its directory — never more (ADR 0001
    /// ownership).
    pub fn delete(&self, slug: &str) -> Result<(), StorageError> {
        if !slug::is_valid_slug(slug) {
            return Err(StorageError::Invalid(format!(
                "invalid prefix slug {slug:?}"
            )));
        }
        self.storage.delete_prefix(slug)
    }
}

/// The tree-health doctor check (blueprint §7: the doctor is the check phase
/// applied tree-wide). This slice checks the tree; runner-integrity and
/// wrapper-runtime sections land with their slices (#28+).
pub struct DoctorService<S: Storage> {
    storage: S,
}

impl<S: Storage> DoctorService<S> {
    /// The service over one storage adapter.
    pub fn new(storage: S) -> Self {
        Self { storage }
    }

    /// The tree health report: root presence, required directories and files,
    /// invalid hand-edits, orphan prefix dirs. No side effects — the doctor
    /// reports what is on disk, it never initializes or repairs.
    pub fn tree_health(&self) -> Result<TreeHealth, StorageError> {
        self.storage.tree_health()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ArtifactKind, DoctorService, InstallError, InstallOutcome, InstallResult, InstallService,
        LaunchApp, PrefixService, Storage,
    };

    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use cellar_core::entities::{AppEntry, AppKind, Candidate, Overrides, Settings};
    use cellar_core::errors::{ResolveError, StorageError};
    use cellar_core::health::TreeHealth;
    use cellar_core::manifest::RunnerManifest;
    use cellar_core::ports::{__sealed, RunnerResolver};
    use cellar_core::types::{
        ProviderMode, ResolvedRunner, RunnerFamily, RunnerInstall, RunnerRef, RunnerSpec,
    };
    use cellar_core::{Prefix, PrefixDefaults};
    use cellar_launch::{LaunchError, LaunchMode};

    /// In-memory `Storage` double: real registry state (apps and prefixes
    /// live here, saves and deletes mutate it), canned tree health, and
    /// call recordings for the service calls. `Mutex` interior so the double
    /// meets the port's `Send + Sync` bound.
    #[derive(Debug)]
    struct MockStorage {
        created: Mutex<Vec<String>>,
        deleted: Mutex<Vec<String>>,
        prefixes: Mutex<Vec<Prefix>>,
        apps: Mutex<Vec<AppEntry>>,
        /// App slugs claimed by hand-edited files — the dedupe domain.
        taken_app_slugs: Mutex<Vec<String>>,
        /// Prefix slugs whose file is broken (loads yield `Invalid`).
        broken_prefixes: Mutex<Vec<String>>,
        canonicalized: Mutex<Vec<PathBuf>>,
        /// Exe paths that fail the launch check (canonicalize → `NotFound`).
        missing_exes: Mutex<Vec<PathBuf>>,
        /// The canned discovery scan (the flat scan's result for the
        /// session's running/extracting branches).
        candidates: Mutex<Vec<Candidate>>,
        /// Where per-launch logs go (the real store: the tree's
        /// `cache/launch-logs`); spawn tests point it at a temp dir.
        log_dir: PathBuf,
        /// The prefix layout base — extraction tests point it at a temp dir
        /// so the archive branch writes real files.
        prefix_base: PathBuf,
        health: TreeHealth,
    }

    impl MockStorage {
        fn new(health: TreeHealth) -> Self {
            Self {
                created: Mutex::new(Vec::new()),
                deleted: Mutex::new(Vec::new()),
                prefixes: Mutex::new(Vec::new()),
                apps: Mutex::new(Vec::new()),
                taken_app_slugs: Mutex::new(Vec::new()),
                broken_prefixes: Mutex::new(Vec::new()),
                canonicalized: Mutex::new(Vec::new()),
                missing_exes: Mutex::new(Vec::new()),
                candidates: Mutex::new(Vec::new()),
                log_dir: PathBuf::from("/mock/cache/launch-logs"),
                prefix_base: PathBuf::from("/mock/prefixes"),
                health,
            }
        }

        fn with_log_dir(mut self, log_dir: PathBuf) -> Self {
            self.log_dir = log_dir;
            self
        }

        fn with_prefix_base(mut self, prefix_base: PathBuf) -> Self {
            self.prefix_base = prefix_base;
            self
        }

        /// Canned discovery candidates the session presents after a run.
        fn push_candidate(&self, candidate: Candidate) {
            self.candidates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(candidate);
        }

        fn created(&self) -> Vec<String> {
            self.created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn deleted(&self) -> Vec<String> {
            self.deleted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn canonicalized(&self) -> Vec<PathBuf> {
            self.canonicalized
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn add_app(&self, app: AppEntry) {
            self.apps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(app);
        }

        fn add_prefix(&self, prefix: Prefix) {
            self.prefixes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(prefix);
        }

        fn take_app_slugs(&self, slugs: &[&str]) {
            *self
                .taken_app_slugs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                slugs.iter().map(|s| (*s).to_owned()).collect();
        }

        fn mark_broken_prefix(&self, slug: &str) {
            self.broken_prefixes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(slug.to_owned());
        }

        fn mark_exe_missing(&self, path: &Path) {
            self.missing_exes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(path.to_path_buf());
        }
    }

    impl cellar_core::ports::__sealed::Sealed for MockStorage {}

    impl Storage for MockStorage {
        fn data_root(&self) -> &Path {
            Path::new("/mock")
        }

        fn prefix_dir(&self, slug: &str) -> PathBuf {
            self.prefix_base.join(slug)
        }

        fn launch_logs_dir(&self) -> PathBuf {
            self.log_dir.clone()
        }

        fn load_settings(&self) -> Result<Settings, StorageError> {
            Ok(Settings::default())
        }

        fn save_settings(&self, _settings: &Settings) -> Result<(), StorageError> {
            Ok(())
        }

        fn create_prefix(&self, slug: &str) -> Result<Prefix, StorageError> {
            // Mirrors the real store's dedupe domain: every existing prefix
            // plus every broken hand-edited one — the fresh sibling's slug
            // comes back, never the requested one when it is taken.
            let mut prefixes = self
                .prefixes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let broken = self
                .broken_prefixes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let taken: BTreeSet<String> = prefixes
                .iter()
                .map(|prefix| prefix.slug.clone())
                .chain(broken.iter().cloned())
                .collect();
            let prefix = Prefix {
                slug: cellar_core::slug::dedupe_slug(slug, &taken),
                defaults: PrefixDefaults::default(),
            };
            self.created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(slug.to_owned());
            prefixes.push(prefix.clone());
            Ok(prefix)
        }

        fn list_prefixes(&self) -> Result<Vec<Prefix>, StorageError> {
            Ok(Vec::new())
        }

        fn load_prefix(&self, slug: &str) -> Result<Prefix, StorageError> {
            if self
                .broken_prefixes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|broken| broken == slug)
            {
                return Err(StorageError::Invalid(format!("broken prefix {slug}")));
            }
            self.prefixes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .find(|prefix| prefix.slug == slug)
                .cloned()
                .ok_or_else(|| StorageError::NotFound(format!("prefix {slug}")))
        }

        fn save_prefix(&self, _prefix: &Prefix) -> Result<(), StorageError> {
            Ok(())
        }

        fn delete_prefix(&self, slug: &str) -> Result<(), StorageError> {
            self.deleted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(slug.to_owned());
            Ok(())
        }

        fn list_apps(&self) -> Result<Vec<AppEntry>, StorageError> {
            Ok(self
                .apps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone())
        }

        fn load_app(&self, slug: &str) -> Result<AppEntry, StorageError> {
            self.apps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .find(|app| app.slug == slug)
                .cloned()
                .ok_or_else(|| StorageError::NotFound(format!("app {slug}")))
        }

        fn save_app(&self, app: &AppEntry) -> Result<(), StorageError> {
            let mut apps = self
                .apps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = apps.iter_mut().find(|a| a.slug == app.slug) {
                *existing = app.clone();
            } else {
                apps.push(app.clone());
            }
            Ok(())
        }

        fn delete_app(&self, slug: &str) -> Result<(), StorageError> {
            let mut apps = self
                .apps
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let before = apps.len();
            apps.retain(|app| app.slug != slug);
            if apps.len() == before {
                Err(StorageError::NotFound(format!("app {slug}")))
            } else {
                Ok(())
            }
        }

        fn list_app_slugs(&self) -> Result<Vec<String>, StorageError> {
            Ok(self
                .taken_app_slugs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone())
        }

        fn canonicalize_exe(&self, path: &Path) -> Result<PathBuf, StorageError> {
            let missing = self
                .missing_exes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if missing.iter().any(|gone| gone == path) {
                return Err(StorageError::NotFound(path.display().to_string()));
            }
            drop(missing);
            self.canonicalized
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(path.to_path_buf());
            Ok(path.to_path_buf())
        }

        fn tree_health(&self) -> Result<TreeHealth, StorageError> {
            Ok(self.health.clone())
        }

        fn discover_executables(&self, _prefix: &Prefix) -> Result<Vec<Candidate>, StorageError> {
            Ok(self
                .candidates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone())
        }

        fn install_managed(&self, _manifest: &RunnerManifest) -> Result<PathBuf, StorageError> {
            Err(StorageError::Unimplemented("mock".to_owned()))
        }
    }

    fn healthy_tree() -> TreeHealth {
        TreeHealth {
            root: PathBuf::from("/mock"),
            tree_exists: true,
            missing_dirs: Vec::new(),
            missing_files: Vec::new(),
            invalid_files: Vec::new(),
            orphan_prefix_dirs: Vec::new(),
            missing_exes: Vec::new(),
            schema_version: 1,
        }
    }

    #[test]
    fn create_slugifies_the_name_before_storage() -> Result<(), StorageError> {
        let mock = MockStorage::new(healthy_tree());
        let service = PrefixService::new(mock);
        let created = service.create("My Games")?;
        assert_eq!(created.slug, "my-games");
        assert_eq!(service.storage.created(), ["my-games"]);
        Ok(())
    }

    #[test]
    fn create_rejects_unslugifiable_names_without_touching_storage() {
        let mock = MockStorage::new(healthy_tree());
        let service = PrefixService::new(mock);
        assert!(service.create("!!!").is_err());
        assert!(service.storage.created().is_empty());
    }

    #[test]
    fn delete_validates_before_storage_and_passes_valid_slugs() -> Result<(), StorageError> {
        let mock = MockStorage::new(healthy_tree());
        let service = PrefixService::new(mock);
        assert!(service.delete("../escape").is_err());
        assert!(service.storage.deleted().is_empty());
        service.delete("my-games")?;
        assert_eq!(service.storage.deleted(), ["my-games"]);
        Ok(())
    }

    #[test]
    fn doctor_reports_what_storage_sees() -> Result<(), StorageError> {
        let mut health = healthy_tree();
        health
            .invalid_files
            .push(PathBuf::from("prefixes/broken/prefix.toml"));
        let mock = MockStorage::new(health);
        let service = DoctorService::new(mock);
        let report = service.tree_health()?;
        assert!(!report.is_healthy());
        assert_eq!(
            report.invalid_files,
            [PathBuf::from("prefixes/broken/prefix.toml")]
        );
        Ok(())
    }

    fn entry(slug: &str, exe: &str) -> AppEntry {
        AppEntry {
            slug: slug.to_owned(),
            exe: PathBuf::from(exe),
            kind: AppKind::Game,
            prefix: "default".to_owned(),
            overrides: Overrides::default(),
            runner: None,
            source_installer: None,
            installed_at: None,
        }
    }

    /// The one registration of a standalone session — install tests unwrap
    /// it (sessions may register many once review lands, #31).
    fn registered(outcome: InstallOutcome) -> InstallResult {
        let mut registrations = outcome.registrations;
        assert_eq!(registrations.len(), 1, "expected exactly one registration");
        registrations.pop().expect("length asserted above")
    }

    #[test]
    fn install_registers_a_new_entry_without_executing() -> Result<(), InstallError> {
        let mock = MockStorage::new(healthy_tree());
        let service = InstallService::new(mock, StubResolver::ok());
        let result = registered(service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Game,
            ArtifactKind::Standalone,
        )?);
        assert!(!result.was_update);
        assert_eq!(result.entry.slug, "balatro");
        assert_eq!(result.entry.exe, PathBuf::from("/games/balatro.exe"));
        assert_eq!(result.entry.kind, AppKind::Game);
        assert_eq!(result.entry.prefix, "default");
        assert_eq!(
            service.storage.canonicalized(),
            [PathBuf::from("/games/balatro.exe")],
            "the identity is the canonical exe path"
        );
        assert_eq!(service.storage.created(), ["default"]);
        assert_eq!(service.storage.list_apps()?.len(), 1);
        assert!(
            registered(service.install(
                Path::new("/games/balatro.exe"),
                "default",
                None,
                AppKind::Game,
                ArtifactKind::Standalone,
            )?)
            .was_update,
            "re-install updates; the standalone branch itself is unchanged"
        );
        Ok(())
    }

    #[test]
    fn install_uses_the_flag_name_and_dedupes_the_slug() -> Result<(), InstallError> {
        let mock = MockStorage::new(healthy_tree());
        mock.take_app_slugs(&["my-game", "my-game-2"]);
        let service = InstallService::new(mock, StubResolver::ok());
        let result = registered(service.install(
            Path::new("/games/game.exe"),
            "default",
            Some("My Game"),
            AppKind::Game,
            ArtifactKind::Standalone,
        )?);
        assert_eq!(
            result.entry.slug, "my-game-3",
            "-2 dedupe against the slug domain"
        );
        Ok(())
    }

    #[test]
    fn reinstalling_the_same_exe_updates_the_same_entry() -> Result<(), InstallError> {
        let mock = MockStorage::new(healthy_tree());
        let service = InstallService::new(mock, StubResolver::ok());
        let first = registered(service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Game,
            ArtifactKind::Standalone,
        )?);
        assert!(!first.was_update);
        let second = registered(service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Tool,
            ArtifactKind::Standalone,
        )?);
        assert!(second.was_update);
        assert_eq!(
            second.entry.slug, first.entry.slug,
            "the slug stays the entry's"
        );
        assert_eq!(second.entry.kind, AppKind::Tool, "the new flags land on it");
        assert_eq!(service.storage.list_apps()?.len(), 1, "no second entry");
        assert_eq!(
            service.storage.created().len(),
            1,
            "the prefix is not created twice"
        );
        Ok(())
    }

    #[test]
    fn reinstalling_rebinds_the_prefix() -> Result<(), InstallError> {
        let mock = MockStorage::new(healthy_tree());
        let service = InstallService::new(mock, StubResolver::ok());
        registered(service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Game,
            ArtifactKind::Standalone,
        )?);
        let rebound = registered(service.install(
            Path::new("/games/balatro.exe"),
            "games",
            None,
            AppKind::Game,
            ArtifactKind::Standalone,
        )?);
        assert!(rebound.was_update);
        assert_eq!(rebound.entry.prefix, "games");
        assert_eq!(service.storage.list_apps()?.len(), 1);
        Ok(())
    }

    #[test]
    fn install_uses_an_existing_prefix_without_creating_it() -> Result<(), InstallError> {
        let mock = MockStorage::new(healthy_tree());
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults::default(),
        });
        let service = InstallService::new(mock, StubResolver::ok());
        registered(service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Game,
            ArtifactKind::Standalone,
        )?);
        assert!(service.storage.created().is_empty());
        Ok(())
    }

    #[test]
    fn install_sidesteps_a_broken_hand_edited_prefix() -> Result<(), InstallError> {
        // A broken prefix.toml is never clobbered (ADR 0001): the install
        // binds to a freshly deduped sibling — the same dedupe `prefix
        // create` applies, so the requested slug is never silently reused.
        let mock = MockStorage::new(healthy_tree());
        mock.mark_broken_prefix("default");
        let service = InstallService::new(mock, StubResolver::ok());
        let result = registered(service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Game,
            ArtifactKind::Standalone,
        )?);
        assert_eq!(service.storage.created(), ["default"]);
        assert_eq!(result.entry.prefix, "default-2", "the fresh sibling binds");
        Ok(())
    }

    #[test]
    fn install_rejects_bad_inputs_before_touching_the_tree() -> Result<(), InstallError> {
        let mock = MockStorage::new(healthy_tree());
        let service = InstallService::new(mock, StubResolver::ok());
        // A non-exe is not a standalone or installer artifact.
        assert!(matches!(
            service.install(
                Path::new("/games/readme.txt"),
                "default",
                None,
                AppKind::Game,
                ArtifactKind::Standalone
            ),
            Err(InstallError::Storage(StorageError::Invalid(_)))
        ));
        // An escaping prefix slug is rejected up front.
        assert!(matches!(
            service.install(
                Path::new("/games/balatro.exe"),
                "../escape",
                None,
                AppKind::Game,
                ArtifactKind::Installer
            ),
            Err(InstallError::Storage(StorageError::Invalid(_)))
        ));
        // An unslugifiable display name cannot name an entry.
        assert!(matches!(
            service.install(
                Path::new("/games/balatro.exe"),
                "default",
                Some("!!!"),
                AppKind::Game,
                ArtifactKind::Standalone
            ),
            Err(InstallError::Storage(StorageError::Invalid(_)))
        ));
        assert!(
            service.storage.list_apps()?.is_empty(),
            "nothing registered"
        );
        assert!(service.storage.created().is_empty(), "no prefix created");
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn installer_branch_runs_the_installer_awaits_its_exit_and_presents_candidates()
    -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cellar-app-installer-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir)?;
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs)?;
        // The configured stub runner plays wine; the installer artifact
        // itself needs no real file — the mock canonicalizes by echo.
        let wine = dir.join("stub-wine");
        write_stub_script(&wine, "echo \"install-line\"\nexit 0\n")?;
        let mock = MockStorage::new(healthy_tree())
            .with_log_dir(logs.clone())
            .with_prefix_base(dir.join("prefixes"));
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults {
                runner: Some(RunnerSpec::new(RunnerFamily::Wine)),
                ..PrefixDefaults::default()
            },
        });
        let dropped = Candidate {
            exe: dir.join("prefixes/default/drive_c/users/me/Desktop/game.exe"),
            label: "game".to_owned(),
        };
        mock.push_candidate(dropped.clone());
        let service = InstallService::new(mock, StubResolver::new(Ok(wine_resolved_at(&wine))));
        let outcome = service.install(
            Path::new("/tmp/setup.exe"),
            "default",
            None,
            AppKind::Game,
            ArtifactKind::Installer,
        )?;
        assert!(outcome.registrations.is_empty(), "nothing registers yet");
        assert_eq!(outcome.prefix_slug, "default");
        let log = outcome.log_path.expect("the installer run has a log");
        assert!(
            log.starts_with(&logs),
            "the run's output lands under the disposable cache"
        );
        let text = std::fs::read_to_string(&log)?;
        assert!(text.contains("install-line"), "run output missing:\n{text}");
        assert_eq!(
            outcome.candidates,
            [dropped],
            "the flat scan's candidates are presented for review"
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn installer_failure_aborts_the_session() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cellar-app-installer-fail-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir)?;
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs)?;
        let wine = dir.join("stub-wine");
        write_stub_script(&wine, "exit 7\n")?;
        let mock = MockStorage::new(healthy_tree())
            .with_log_dir(dir.join("logs"))
            .with_prefix_base(dir.join("prefixes"));
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults {
                runner: Some(RunnerSpec::new(RunnerFamily::Wine)),
                ..PrefixDefaults::default()
            },
        });
        let service = InstallService::new(mock, StubResolver::new(Ok(wine_resolved_at(&wine))));
        let err = service
            .install(
                Path::new("/tmp/setup.exe"),
                "default",
                None,
                AppKind::Game,
                ArtifactKind::Installer,
            )
            .expect_err("a failed installer aborts the session");
        assert!(
            matches!(
                &err,
                InstallError::InstallerFailed {
                    code: Some(7),
                    signal: None
                }
            ),
            "the exit code is reported raw: {err}"
        );
        assert!(
            err.to_string().contains("the installer failed"),
            "the disposition names the failure: {err}"
        );
        Ok(())
    }

    #[test]
    fn installer_runs_under_the_prefix_runner_or_the_tool_floor() {
        // The artifact run is a tool operation: the prefix's runner default
        // when pinned, else the Tool floor (wine) — never the Game floor
        // (Proton), which would demand a managed runner for a setup.exe.
        // The resolver fails before any spawn; the selection walk's spec is
        // what the test records (the stub records it before failing).
        let seek_spec = |pinned: Option<RunnerSpec>| -> Result<RunnerSpec, String> {
            let mock = MockStorage::new(healthy_tree());
            mock.add_prefix(Prefix {
                slug: "default".to_owned(),
                defaults: PrefixDefaults {
                    runner: pinned,
                    ..PrefixDefaults::default()
                },
            });
            let resolver = StubResolver::new(Err(ResolveError::Unresolvable {
                family: RunnerFamily::Wine,
            }));
            let service = InstallService::new(mock, resolver);
            let _ = service
                .install(
                    Path::new("/tmp/setup.exe"),
                    "default",
                    None,
                    AppKind::Game,
                    ArtifactKind::Installer,
                )
                .expect_err("resolution fails before any spawn");
            service
                .resolver
                .seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .ok_or_else(|| "the selection walk never ran".to_owned())
        };
        let pinned = seek_spec(Some(RunnerSpec::new(RunnerFamily::Proton)))
            .expect("the selection walk records the spec");
        assert_eq!(
            pinned.family,
            RunnerFamily::Proton,
            "the prefix default pins"
        );
        let floor = seek_spec(None).expect("the selection walk records the spec");
        assert_eq!(floor.family, RunnerFamily::Wine, "the tool floor applies");
    }

    #[test]
    #[cfg(unix)]
    fn archive_branch_extracts_into_the_prefix_and_presents_candidates() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cellar-app-archive-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir)?;
        let bundle = dir.join("bundle.zip");
        crate::archive::build_zip(
            &bundle,
            &[
                ("game/Game.exe", "MZ"),
                ("game/data/level.bin", "level"),
                ("readme.txt", "hi"),
            ],
        );
        let mock = MockStorage::new(healthy_tree()).with_prefix_base(dir.join("prefixes"));
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults::default(),
        });
        let candidate = Candidate {
            exe: dir.join("prefixes/default/drive_c/game/Game.exe"),
            label: "Game".to_owned(),
        };
        mock.push_candidate(candidate.clone());
        let service = InstallService::new(mock, StubResolver::ok());
        let outcome = service.install(
            &bundle,
            "default",
            None,
            AppKind::Game,
            ArtifactKind::Archive,
        )?;
        assert!(outcome.registrations.is_empty());
        assert_eq!(outcome.log_path, None, "no run, no log");
        // The archive landed at the prefix's wine root, structure intact.
        let drive_c = dir.join("prefixes/default/drive_c");
        assert_eq!(
            std::fs::read(drive_c.join("game/Game.exe")).unwrap_or_default(),
            b"MZ"
        );
        assert!(drive_c.join("game/data/level.bin").is_file());
        assert!(drive_c.join("readme.txt").is_file());
        assert_eq!(
            outcome.candidates,
            [candidate],
            "the scan's candidates are presented for review"
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn archive_branch_refuses_traversal() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cellar-app-archive-evil-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir)?;
        let bundle = dir.join("evil.zip");
        crate::archive::build_zip(&bundle, &[("../evil.exe", "MZ")]);
        let mock = MockStorage::new(healthy_tree()).with_prefix_base(dir.join("prefixes"));
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults::default(),
        });
        let service = InstallService::new(mock, StubResolver::ok());
        let err = service
            .install(
                &bundle,
                "default",
                None,
                AppKind::Game,
                ArtifactKind::Archive,
            )
            .expect_err("traversal is refused");
        assert!(
            matches!(
                &err,
                InstallError::Archive(crate::archive::ArchiveError::Traversal { entry })
                    if entry == "../evil.exe"
            ),
            "the refusing entry is named: {err}"
        );
        assert!(
            !dir.join("prefixes").join("evil.exe").exists(),
            "nothing escaped the prefix"
        );
        assert!(!dir.join("prefixes/default/drive_c").exists());
        Ok(())
    }

    #[test]
    fn list_joins_tree_health_for_entry_status() -> Result<(), StorageError> {
        let mut health = healthy_tree();
        health.missing_exes.push("balatro".to_owned());
        let mock = MockStorage::new(health);
        mock.add_app(entry("balatro", "/games/balatro.exe"));
        mock.add_app(entry("warpinator", "/prefix/drive_c/warpinator.exe"));
        let service = InstallService::new(mock, StubResolver::ok());
        let listed = service.list()?;
        let statuses: Vec<_> = listed
            .iter()
            .map(|listed| (listed.entry.slug.as_str(), listed.status.as_str()))
            .collect();
        assert_eq!(
            statuses,
            [("balatro", "missing-exe"), ("warpinator", "ok")],
            "status joins the tree-wide exe check"
        );
        Ok(())
    }

    #[test]
    fn uninstall_removes_only_that_entry() -> Result<(), StorageError> {
        let mock = MockStorage::new(healthy_tree());
        mock.add_app(entry("balatro", "/games/balatro.exe"));
        mock.add_app(entry("warpinator", "/prefix/drive_c/warpinator.exe"));
        let service = InstallService::new(mock, StubResolver::ok());
        service.uninstall("balatro")?;
        let apps = service.storage.list_apps()?;
        let remaining: Vec<_> = apps.iter().map(|app| app.slug.as_str()).collect();
        assert_eq!(
            remaining,
            ["warpinator"],
            "exactly the requested entry is gone"
        );
        assert!(matches!(
            service.uninstall("balatro"),
            Err(StorageError::NotFound(_))
        ));
        assert!(matches!(
            service.uninstall("../escape"),
            Err(StorageError::Invalid(_))
        ));
        Ok(())
    }

    /// A resolver double: a canned outcome plus the last spec it saw —
    /// records what the selection walk produced for the orchestration.
    /// The `seen` record is `Arc`-shared so tests can inspect it after the
    /// resolver has been moved into the service.
    #[derive(Debug)]
    struct StubResolver {
        result: Result<ResolvedRunner, ResolveError>,
        seen: Arc<Mutex<Option<RunnerSpec>>>,
    }

    impl StubResolver {
        fn new(result: Result<ResolvedRunner, ResolveError>) -> Self {
            Self {
                result,
                seen: Arc::new(Mutex::new(None)),
            }
        }

        fn ok() -> Self {
            Self::new(Ok(wine_resolved()))
        }
    }

    impl __sealed::Sealed for StubResolver {}

    impl RunnerResolver for StubResolver {
        fn id(&self) -> &'static str {
            "stub"
        }

        fn resolve(&self, spec: &RunnerSpec) -> Result<ResolvedRunner, ResolveError> {
            *self
                .seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(spec.clone());
            self.result.clone()
        }
    }

    /// The canonical shape a wine resolution returns: discover-only, found
    /// on PATH.
    fn wine_resolved() -> ResolvedRunner {
        ResolvedRunner {
            mode: ProviderMode::DiscoverOnly,
            reference: RunnerRef {
                provider_id: "wine".to_owned(),
                family: RunnerFamily::Wine,
                install: RunnerInstall::Discovered {
                    path: PathBuf::from("/usr/bin/wine"),
                    version: None,
                },
            },
        }
    }

    #[test]
    fn launch_plans_a_registered_tool_end_to_end() -> Result<(), LaunchError> {
        let mock = MockStorage::new(healthy_tree());
        mock.add_app(entry("balatro", "/games/balatro.exe"));
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults {
                runner: Some(RunnerSpec::new(RunnerFamily::Wine)),
                ..PrefixDefaults::default()
            },
        });
        let service = LaunchApp::new(mock, StubResolver::ok());
        let plan = service.plan("balatro", &["-x".to_owned()])?;
        assert_eq!(
            plan.argv,
            [
                "/usr/bin/wine".to_owned(),
                "/games/balatro.exe".to_owned(),
                "-x".to_owned(),
            ]
        );
        assert_eq!(
            plan.env.get("WINEPREFIX").map(String::as_str),
            Some("/mock/prefixes/default"),
            "the plan pins the bound prefix for wine"
        );
        assert!(plan.wrappers.is_empty());
        Ok(())
    }

    #[test]
    fn launch_picks_the_defaults_of_the_binding_override_prefix() {
        let mock = MockStorage::new(healthy_tree());
        let mut overridden = entry("balatro", "/games/balatro.exe");
        overridden.overrides.prefix = Some("games".to_owned());
        mock.add_app(overridden);
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults::default(),
        });
        mock.add_prefix(Prefix {
            slug: "games".to_owned(),
            defaults: PrefixDefaults {
                runner: Some(RunnerSpec::new(RunnerFamily::Proton)),
                ..PrefixDefaults::default()
            },
        });
        let seen = Arc::new(Mutex::new(None));
        let service = LaunchApp::new(
            mock,
            StubResolver {
                result: Ok(wine_resolved()),
                seen: Arc::clone(&seen),
            },
        );
        let plan = service
            .plan("balatro", &[])
            .unwrap_or_else(|e| panic!("plan: {e}"));
        assert_eq!(
            *seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some(RunnerSpec::new(RunnerFamily::Proton)),
            "the binding override selects whose defaults apply"
        );
        assert_eq!(
            plan.env.get("WINEPREFIX").map(String::as_str),
            Some("/mock/prefixes/games"),
            "the plan pins the override-bound prefix"
        );
    }

    #[test]
    fn launch_resolution_failures_map_to_the_taxonomy() {
        let mock = MockStorage::new(healthy_tree());
        mock.add_app(entry("balatro", "/games/balatro.exe"));
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults::default(),
        });
        let service = LaunchApp::new(
            mock,
            StubResolver::new(Err(ResolveError::Unresolvable {
                family: RunnerFamily::Wine,
            })),
        );
        let err = service.plan("balatro", &[]).expect_err("no wine available");
        assert_eq!(
            err,
            LaunchError::Resolve(ResolveError::Unresolvable {
                family: RunnerFamily::Wine
            }),
            "resolution exhausted maps to the resolve family"
        );
        assert!(
            err.to_string().contains("install it"),
            "the message carries the SuggestInstall disposition: {err}"
        );
    }

    #[test]
    fn launch_missing_exe_maps_to_registration_disposition() {
        let mock = MockStorage::new(healthy_tree());
        mock.add_app(entry("balatro", "/games/balatro.exe"));
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults::default(),
        });
        mock.mark_exe_missing(Path::new("/games/balatro.exe"));
        let service = LaunchApp::new(mock, StubResolver::ok());
        let err = service.plan("balatro", &[]).expect_err("exe deleted");
        assert_eq!(
            err,
            LaunchError::ExeMissing {
                slug: "balatro".to_owned(),
                exe: PathBuf::from("/games/balatro.exe"),
            }
        );
        assert!(
            err.to_string().contains("re-register"),
            "the message carries the re-register disposition: {err}"
        );
    }

    #[test]
    fn launch_missing_prefix_maps_to_recreate_disposition() {
        let mock = MockStorage::new(healthy_tree());
        mock.add_app(entry("balatro", "/games/balatro.exe"));
        // No prefix registered — the binding's prefix is gone.
        let service = LaunchApp::new(mock, StubResolver::ok());
        let err = service.plan("balatro", &[]).expect_err("prefix gone");
        assert_eq!(
            err,
            LaunchError::PrefixMissing {
                slug: "default".to_owned()
            }
        );
        assert!(
            err.to_string().contains("recreate"),
            "the message carries the recreate disposition: {err}"
        );
    }

    #[test]
    fn launch_unknown_app_maps_to_app_not_found() {
        let mock = MockStorage::new(healthy_tree());
        let service = LaunchApp::new(mock, StubResolver::ok());
        let err = service.plan("nope", &[]).expect_err("nothing registered");
        assert_eq!(
            err,
            LaunchError::AppNotFound {
                slug: "nope".to_owned()
            }
        );
        assert!(
            err.to_string().contains("register it"),
            "the message carries the registration hint"
        );
    }

    /// A resolved wine runner at an explicit path — spawn tests point it at
    /// a real stub executable.
    fn wine_resolved_at(path: &Path) -> ResolvedRunner {
        ResolvedRunner {
            mode: ProviderMode::DiscoverOnly,
            reference: RunnerRef {
                provider_id: "wine".to_owned(),
                family: RunnerFamily::Wine,
                install: RunnerInstall::Discovered {
                    path: path.to_path_buf(),
                    version: None,
                },
            },
        }
    }

    /// Write an executable stub script the way every spawn test does:
    /// create → write → `sync_all` → drop, so the script leaves the
    /// write-open state (the exec `ETXTBSY` window) before any spawn — one
    /// pattern, no drifted copies (mirrored in `cellar-launch` and the CLI).
    #[cfg(unix)]
    fn write_stub_script(path: &Path, body: &str) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let mut file = std::fs::File::create(path)?;
        file.write_all(format!("#!/bin/sh\n{body}\n").as_bytes())?;
        file.sync_all()?;
        drop(file);
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)
    }

    #[test]
    #[cfg(unix)]
    fn launch_spawns_the_frozen_plan_into_the_per_launch_log() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cellar-app-launch-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir)?;
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs)?;
        // The configured stub runner: echo both streams, exit 7 — the
        // configured path wins resolution, so no real wine is needed.
        let wine = dir.join("stub-wine");
        write_stub_script(&wine, "echo \"app-out\"\necho \"app-err\" >&2\nexit 7\n")?;
        let mock = MockStorage::new(healthy_tree()).with_log_dir(logs.clone());
        mock.add_app(entry("balatro", "/games/balatro.exe"));
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults {
                runner: Some(RunnerSpec::new(RunnerFamily::Wine)),
                ..PrefixDefaults::default()
            },
        });
        let service = LaunchApp::new(mock, StubResolver::new(Ok(wine_resolved_at(&wine))));
        let process = service.spawn("balatro", &[], LaunchMode::Foreground)?;
        assert!(process.pid() > 0);
        // `wait` consumes the handle; the log path is wanted for the rest
        // of the assertions.
        let log_path = process.log_path().to_path_buf();
        let status = process.wait()?;
        assert_eq!(status.code(), Some(7), "the exit code propagates raw");
        assert!(
            log_path.starts_with(&logs),
            "the log lives under the disposable cache: {}",
            log_path.display()
        );
        let name = log_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        assert!(
            name.starts_with("balatro-"),
            "the log is <slug>-<timestamp>.log, got {name}"
        );
        assert_eq!(
            log_path.extension().and_then(|ext| ext.to_str()),
            Some("log"),
            "the log ends in .log: {name}"
        );
        let text = std::fs::read_to_string(log_path)?;
        assert!(text.contains("app-out"), "stdout missing:\n{text}");
        assert!(text.contains("app-err"), "stderr missing:\n{text}");
        Ok(())
    }

    #[test]
    fn launch_checks_in_taxonomy_order_resolve_before_exe() {
        // Both a dead exe and an unresolvable runner: the resolve phase
        // reports first (blueprint §7 phase order, deterministic first
        // failure wins).
        let mock = MockStorage::new(healthy_tree());
        mock.add_app(entry("balatro", "/games/balatro.exe"));
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults::default(),
        });
        mock.mark_exe_missing(Path::new("/games/balatro.exe"));
        let service = LaunchApp::new(
            mock,
            StubResolver::new(Err(ResolveError::Unresolvable {
                family: RunnerFamily::Proton,
            })),
        );
        let err = service.plan("balatro", &[]).expect_err("runner first");
        assert!(
            matches!(err, LaunchError::Resolve(_)),
            "the resolve family reports before the check family: {err:?}"
        );
    }

    #[test]
    fn list_joins_the_bound_prefix_default_runner() -> Result<(), StorageError> {
        let mock = MockStorage::new(healthy_tree());
        mock.add_app(entry("balatro", "/games/balatro.exe"));
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults {
                runner: Some(RunnerSpec::new(RunnerFamily::Proton)),
                ..PrefixDefaults::default()
            },
        });
        let service = InstallService::new(mock, StubResolver::ok());
        let listed = service.list()?;
        assert_eq!(
            listed[0].prefix_runner,
            Some(RunnerSpec::new(RunnerFamily::Proton)),
            "the runner column's prefix-default rung joins the chain"
        );
        Ok(())
    }

    #[test]
    fn list_degrades_the_runner_column_when_the_prefix_is_broken() -> Result<(), StorageError> {
        let mock = MockStorage::new(healthy_tree());
        mock.add_app(entry("balatro", "/games/balatro.exe"));
        mock.mark_broken_prefix("default");
        let service = InstallService::new(mock, StubResolver::ok());
        let listed = service.list()?;
        assert_eq!(
            listed[0].prefix_runner, None,
            "a broken prefix degrades the column to the floor — the doctor flags it"
        );
        Ok(())
    }
}
