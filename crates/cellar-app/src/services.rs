//! Application use-cases: the `AppEntry` registry — install with the
//! three-armed artifact handling (standalone registers without executing,
//! installer runs inside the prefix with its exit awaited, archive extracts
//! into it — each followed by discovery) and the review registration step
//! (#31): the confirmed keep-list and manual adds of a session become
//! entries, zero or more, all bound to the session's one prefix — app list
//! with per-entry status, uninstall degrading to entry removal (#27), the
//! prefix lifecycle and the tree-health doctor check (#26), and the
//! `LaunchApp` use-case (#28/#29): resolve → check → plan → execute. Thin
//! orchestration over the `core` ports — concrete adapters are injected
//! only at the composition root.
//!
//! Desktop integration lands with #33: registration creates the app's
//! launcher entry and icon (uninstall removes them), a re-registration
//! with a different name renames the app — the identity stays the exe
//! path, the entry file name follows the new slug — and
//! [`DesktopSync`] re-derives everything from the tree. The lifecycle is
//! one-way: the desktop adapter derives host artifacts from the entries
//! it is handed and never writes tree state.

use cellar_core::Prefix;
use cellar_core::entities::{AppEntry, AppKind, Candidate, Overrides};
use cellar_core::errors::DesktopError;
use cellar_core::errors::StorageError;
use cellar_core::health::TreeHealth;
use cellar_core::ports::{DesktopIntegrator, RunnerResolver, Storage};
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
/// touched, the artifact it handled, and the entries registered — the
/// running/extracting branches collect the prefix's menu/desktop
/// candidates for review; registration of the reviewed list is the
/// session's closing step ([`InstallService::register_reviewed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    /// The prefix the session touched — the bound one, created when missing.
    pub prefix_slug: String,
    pub registrations: Vec<InstallResult>,
    /// Executable candidates found after the artifact ran or extracted
    /// (flat scan joined with `.lnk` targets, #31). Empty for standalone.
    pub candidates: Vec<Candidate>,
    /// The artifact-run log under the disposable cache (installer branch —
    /// the run's output always lands in a per-launch log, blueprint §7).
    pub log_path: Option<PathBuf>,
    /// The canonical artifact path this session handled — recorded as the
    /// `source_installer` metadata of entries registered from its review
    /// (installer/archive branches). None for standalone.
    pub artifact: Option<PathBuf>,
}

/// The artifact branch of `cellar install` (blueprint §8 step 2): how the
/// artifact is handled inside the session. The interactive question with
/// its filename-hint default lives at the presentation layer (#31) — the
/// branch is never guessed silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// Register the exe without executing anything (the #27 branch).
    Standalone,
    /// Run the installer inside the bound prefix, awaiting its exit.
    Installer,
    /// Extract the archive into the bound prefix.
    Archive,
}

impl ArtifactKind {
    /// The flag/choice vocabulary (`standalone`, `installer`, `archive`)
    /// — the same strings [`FromStr`] accepts, so prompting and parsing
    /// can never drift.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Installer => "installer",
            Self::Archive => "archive",
        }
    }
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
/// and launch taxonomies passed through, the installer's own exit, archive
/// extraction, and the desktop side effects — each in its own vocabulary,
/// with the fix where the pipeline defines one.
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
    /// The launcher entry or cached icon could not be written or removed
    /// (the derived side of a registration; the tree state is already
    /// saved — re-run the command, or `desktop sync` to re-derive).
    Desktop(DesktopError),
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

impl From<DesktopError> for InstallError {
    fn from(error: DesktopError) -> Self {
        Self::Desktop(error)
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
            Self::Desktop(error) => write!(f, "desktop: {error}"),
        }
    }
}

impl std::error::Error for InstallError {}

/// The `AppEntry` registry and install session (blueprint §8: `cellar
/// install <path>` is the flagship flow). Identity is the canonical exe
/// path — re-installing the same exe updates the same entry (blueprint §6,
/// §8); the slug is the display/file name with `-2` dedupe. The closed
/// three-armed artifact handling (blueprint §5) landed with #30 —
/// standalone, installer (run inside the bound prefix, exit awaited),
/// archive (extract into it); the discovery review and multi-registration
/// land here (#31): [`InstallService::register_reviewed`] confirms the
/// session's candidates — zero or more entries, one prefix, nothing silent.
///
/// Desktop integration (#33): every registration also derives the app's
/// launcher entry and cached icon through the injected
/// [`DesktopIntegrator`] — created on registration, removed on uninstall,
/// refreshed when a re-registration renames the app. The adapter writes
/// derived host artifacts only; tree state stays this service's alone
/// (the lifecycle is one-way).
pub struct InstallService<S: Storage, R: RunnerResolver, D: DesktopIntegrator> {
    storage: S,
    resolver: R,
    desktop: D,
}

impl<S: Storage, R: RunnerResolver, D: DesktopIntegrator> InstallService<S, R, D> {
    /// The service over one storage adapter, one resolver, and one
    /// desktop integrator — the composition root injects the concrete
    /// registry composite (#28) and adapter (#33).
    pub fn new(storage: S, resolver: R, desktop: D) -> Self {
        Self {
            storage,
            resolver,
            desktop,
        }
    }

    /// The flagship flow's artifact handling (blueprint §8 steps 1–3): bind
    /// or create the prefix, then handle the artifact per its
    /// [`ArtifactKind`]. Standalone registers without executing (the #27
    /// branch); installer runs inside the prefix with its exit awaited — a
    /// failed installer aborts the session with
    /// [`InstallError::InstallerFailed`]; archive extracts into the
    /// prefix's wine root, path-traversal-safe. The running/extracting
    /// branches then collect the prefix's menu/desktop executable
    /// candidates for review; the review's confirmation registers them
    /// ([`InstallService::register_reviewed`]) — nothing auto-registers.
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
                let result = self.register_standalone(
                    &canonical,
                    &prefix_slug,
                    kind,
                    base,
                    // A re-registration renames only when the user named
                    // it — the `--name` flag (US26); the stem-derived
                    // default keeps the entry's name.
                    name.is_some(),
                )?;
                Ok(InstallOutcome {
                    prefix_slug,
                    registrations: vec![result],
                    candidates: Vec::new(),
                    log_path: None,
                    artifact: None,
                })
            }
            ArtifactKind::Installer => {
                let (log_path, candidates) = self.run_installer(&canonical, &bound)?;
                Ok(InstallOutcome {
                    prefix_slug,
                    registrations: Vec::new(),
                    candidates,
                    log_path: Some(log_path),
                    artifact: Some(canonical),
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
                    artifact: Some(canonical),
                })
            }
        }
    }

    /// The registration step of an [`InstallSession`](crate) (blueprint §8
    /// step 3 → 4): the review's confirmed keep-list and manual adds become
    /// `AppEntry`s, every one bound to the session's single prefix
    /// (`session.prefix_slug`) — the glossary's one-session-one-prefix
    /// rule, enforced here by construction. Zero or more entries register:
    /// an empty review registers nothing — Cellar never guesses a "main"
    /// exe and never registers silently. Identity is the canonical exe
    /// path (blueprint §6): re-installing an already-registered exe
    /// updates the same entry. Every exe is pre-flighted before the first
    /// write — a missing manual add or an unslugifiable label aborts the
    /// whole review, never a partial registration.
    ///
    /// Each registration also derives the entry's launcher artifacts
    /// (desktop integration, #33): any failure surfaces after the tree
    /// write — re-run the command or `desktop sync` to re-derive.
    pub fn register_reviewed(
        &self,
        session: &InstallOutcome,
        keep: &[Candidate],
        add: &[PathBuf],
        kind: AppKind,
    ) -> Result<Vec<InstallResult>, InstallError> {
        // Pre-flight: canonicalize every exe (the identity and existence
        // check), dedupe within the review, and judge every slug — all
        // before anything is written.
        let mut planned: Vec<(PathBuf, String)> = Vec::new();
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        let mut plan = |exe: &Path, label: &str| -> Result<(), StorageError> {
            let canonical = self.storage.canonicalize_exe(exe)?;
            if !seen.insert(canonical.clone()) {
                return Ok(());
            }
            let base = slug::slugify(label);
            if base.is_empty() {
                return Err(StorageError::Invalid(format!(
                    "cannot form an app slug from {label:?}"
                )));
            }
            planned.push((canonical, base));
            Ok(())
        };
        for candidate in keep {
            plan(&candidate.exe, &candidate.label)?;
        }
        for path in add {
            let label = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            plan(path, &label)?;
        }
        let mut results = Vec::with_capacity(planned.len());
        for (canonical, base) in planned {
            results.push(self.register_one(
                &canonical,
                &session.prefix_slug,
                kind,
                &base,
                session.artifact.clone(),
                // The review's label is the user's confirmed name for the
                // exe — an update brings the entry's name in line.
                true,
            )?);
        }
        Ok(results)
    }

    /// The standalone branch (blueprint §8: register without executing) —
    /// the #27 logic, unchanged. Re-installing the same exe updates the
    /// same entry — its slug and identity stay, its kind and prefix
    /// binding take the new flags. A rename only when the user names it:
    /// the `--name` flag is the register-time rename channel (blueprint
    /// §6: renaming renames the file, identity stays the exe) — the
    /// stem-derived default name never renames an entry (US26). The
    /// display name was already judged usable by the session preflight
    /// (no side effects on a bad name).
    fn register_standalone(
        &self,
        canonical: &Path,
        prefix_slug: &str,
        kind: AppKind,
        base: &str,
        explicit_name: bool,
    ) -> Result<InstallResult, InstallError> {
        self.register_one(canonical, prefix_slug, kind, base, None, explicit_name)
    }

    /// The one-entry registration shared by the standalone branch (#27)
    /// and the session review (#31): identity is the canonical exe path —
    /// an existing entry with the same exe is updated in place (kind,
    /// prefix binding, this session's source artifact when there is one,
    /// and — only when the caller names the update — the display name,
    /// which renames the entry, #33), never duplicated; a fresh entry
    /// takes the deduped slug, the binding prefix, and the source
    /// metadata. Every registration closes with the entry's derived
    /// launcher artifacts: the cached icon and the `.desktop` entry
    /// (created under the current slug, the renamed one removed — the
    /// entry file name tracks the slug). Callers pre-flight: `base` is a
    /// pre-slugified display name and `canonical` a verified exe path.
    fn register_one(
        &self,
        canonical: &Path,
        prefix_slug: &str,
        kind: AppKind,
        base: &str,
        source_installer: Option<PathBuf>,
        rename: bool,
    ) -> Result<InstallResult, InstallError> {
        if base.is_empty() {
            return Err(StorageError::Invalid(
                "cannot register an entry without a display name".to_owned(),
            )
            .into());
        }
        let previous_slug = if let Some(mut existing) = self
            .storage
            .list_apps()?
            .into_iter()
            .find(|app| app.exe == canonical)
        {
            let old_slug = existing.slug.clone();
            existing.kind = kind;
            prefix_slug.clone_into(&mut existing.prefix);
            if source_installer.is_some() {
                existing.source_installer = source_installer;
            }
            existing.slug = if rename {
                self.renamed_slug(&existing, base)?
            } else {
                existing.slug.clone()
            };
            let renamed = existing.slug != old_slug;
            self.storage.save_app(&existing)?;
            if renamed {
                // The new name is saved first; the old file drops after
                // — a crash in between leaves a duplicated entry
                // (visible, removable) rather than a lost one.
                self.storage.delete_app(&old_slug)?;
            }
            let icon = self.desktop.install_icon(&existing.exe)?;
            self.desktop.create_entry(
                &existing,
                (existing.slug != old_slug).then_some(old_slug).as_deref(),
                icon.as_deref(),
            )?;
            return Ok(InstallResult {
                entry: existing,
                was_update: true,
            });
        } else {
            None
        };
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
            source_installer,
            installed_at: None,
        };
        self.storage.save_app(&app)?;
        let icon = self.desktop.install_icon(&app.exe)?;
        self.desktop
            .create_entry(&app, previous_slug, icon.as_deref())?;
        Ok(InstallResult {
            entry: app,
            was_update: false,
        })
    }

    /// The entry's slug after a rename: the display name's slug, deduped
    /// against every other app file stem — the entry's own file is being
    /// replaced, so its own name is never a clash.
    fn renamed_slug(&self, entry: &AppEntry, base: &str) -> Result<String, StorageError> {
        let taken: BTreeSet<String> = self
            .storage
            .list_app_slugs()?
            .into_iter()
            .filter(|taken| taken != &entry.slug)
            .collect();
        Ok(slug::dedupe_slug(base, &taken))
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
    /// (ADR 0001 ownership) — followed by the derived launcher cleanup
    /// (the `.desktop` entry and cached icon, #33): the tree is the
    /// source of truth, so a failed cleanup still leaves a consistent
    /// tree — the stale entry is pruned by the next `desktop sync`. The
    /// Windows-uninstaller path (glossary: Uninstall) degrades to entry
    /// removal for now (#30); Cellar never deletes the app's own files.
    pub fn uninstall(&self, slug: &str) -> Result<(), InstallError> {
        if !slug::is_valid_slug(slug) {
            return Err(StorageError::Invalid(format!("invalid app slug {slug:?}")).into());
        }
        // The app's entry is needed for the cleanup — the entry and icon
        // are derived from it.
        let app = self.storage.load_app(slug)?;
        self.storage.delete_app(slug)?;
        self.desktop.remove_entry(&app)?;
        Ok(())
    }
}

/// The re-derivation use-case (blueprint §6: the cache is re-derivable at
/// any time): every registered app's launcher entry and icon are re-derived
/// from the tree, stale entry files are pruned — a renamed or gone app
/// never leaves an entry behind — and the "Open with Cellar" file
/// association is ensured. The lifecycle stays one-way: the sweep reads
/// the tree (through the storage port) and writes derived host artifacts
/// only; no write path leads back into app state.
pub struct DesktopSync<S: Storage, D: DesktopIntegrator> {
    storage: S,
    desktop: D,
}

/// What a re-derivation did: the entry and icon counts plus the stale
/// entry files removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopSyncReport {
    pub entries: usize,
    pub icons: usize,
    pub removed_entries: Vec<PathBuf>,
}

impl<S: Storage, D: DesktopIntegrator> DesktopSync<S, D> {
    /// The use-case over one storage adapter and one integrator.
    pub fn new(storage: S, desktop: D) -> Self {
        Self { storage, desktop }
    }

    /// Re-derive everything derived: each app's entry and icon, the stale
    /// sweep, then the file association.
    pub fn sync(&self) -> Result<DesktopSyncReport, InstallError> {
        let apps = self.storage.list_apps()?;
        let mut icons = 0usize;
        let mut keep = Vec::with_capacity(apps.len());
        for app in &apps {
            keep.push(app.slug.as_str());
            // The icon's source may be gone (a deleted exe): the entry
            // still re-derives — it is the functional half; the icon
            // comes back once the exe does (US24/25: invalid entries are
            // skipped, not blockers).
            let icon = match self.desktop.install_icon(&app.exe) {
                Ok(Some(icon)) => {
                    icons += 1;
                    Some(icon)
                }
                Ok(None) => None,
                Err(_) => None,
            };
            // A fresh create under the current slug; the stale sweep
            // removes any entry left under an old slug.
            self.desktop.create_entry(app, None, icon.as_deref())?;
        }
        let removed_entries = self.desktop.prune_entries(&keep)?;
        self.desktop.set_file_association()?;
        Ok(DesktopSyncReport {
            entries: apps.len(),
            icons,
            removed_entries,
        })
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
        ArtifactKind, DesktopSync, DoctorService, InstallError, InstallOutcome, InstallResult,
        InstallService, LaunchApp, PrefixService, Storage,
    };

    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use cellar_core::entities::{AppEntry, AppKind, Candidate, Overrides, Settings};
    use cellar_core::errors::{DesktopError, ResolveError, StorageError};
    use cellar_core::health::TreeHealth;
    use cellar_core::manifest::RunnerManifest;
    use cellar_core::ports::{__sealed, DesktopIntegrator, RunnerResolver};
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
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
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
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
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
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
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
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
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
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
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
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
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
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
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
    fn review_registers_every_kept_candidate_bound_to_the_session_prefix() -> anyhow::Result<()> {
        // An installer dropping five exes yields up to five entries — one
        // per confirmed candidate, every one bound to the session's single
        // prefix. No guessing a "main" exe: the review decides all of it.
        let mock = MockStorage::new(healthy_tree());
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
        let candidates = (0..5)
            .map(|i| Candidate {
                exe: PathBuf::from(format!("/prefix/drive_c/users/me/Desktop/app{i}.exe")),
                label: format!("App {i}"),
            })
            .collect::<Vec<_>>();
        let outcome = InstallOutcome {
            prefix_slug: "games".to_owned(),
            registrations: Vec::new(),
            candidates: candidates.clone(),
            log_path: None,
            artifact: Some(PathBuf::from("/tmp/setup.exe")),
        };
        let results = service.register_reviewed(&outcome, &candidates, &[], AppKind::Game)?;
        assert_eq!(results.len(), 5, "five confirmed candidates, five entries");
        assert!(
            results.iter().all(|result| !result.was_update),
            "nothing was pre-registered"
        );
        let apps = service.storage.list_apps()?;
        assert_eq!(apps.len(), 5);
        for (app, candidate) in apps.iter().zip(&candidates) {
            assert_eq!(app.prefix, "games", "every entry binds the session prefix");
            assert_eq!(app.exe, candidate.exe, "identity is the candidate exe");
            assert_eq!(app.kind, AppKind::Game, "the session kind applies");
            assert_eq!(
                app.source_installer.as_deref(),
                Some(Path::new("/tmp/setup.exe")),
                "the session's artifact is recorded as the source"
            );
        }
        let slugs: Vec<&str> = apps.iter().map(|app| app.slug.as_str()).collect();
        assert_eq!(
            slugs,
            ["app-0", "app-1", "app-2", "app-3", "app-4"],
            "each label forms its slug in review order"
        );
        Ok(())
    }

    #[test]
    fn review_registers_nothing_without_confirmation() -> anyhow::Result<()> {
        // The hard rule: candidates alone never register — only the
        // review's explicit decisions do. An empty review is a valid
        // session outcome.
        let mock = MockStorage::new(healthy_tree());
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
        let outcome = InstallOutcome {
            prefix_slug: "default".to_owned(),
            registrations: Vec::new(),
            candidates: vec![Candidate {
                exe: PathBuf::from("/prefix/drive_c/game.exe"),
                label: "game".to_owned(),
            }],
            log_path: None,
            artifact: Some(PathBuf::from("/tmp/setup.exe")),
        };
        assert_eq!(
            service.register_reviewed(&outcome, &[], &[], AppKind::Game)?,
            Vec::new(),
            "zero confirmed candidates, zero entries"
        );
        assert!(
            service.storage.list_apps()?.is_empty(),
            "nothing was registered silently"
        );
        Ok(())
    }

    #[test]
    fn review_manual_adds_register_with_the_exe_stem_label() -> anyhow::Result<()> {
        let mock = MockStorage::new(healthy_tree());
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
        let outcome = InstallOutcome {
            prefix_slug: "default".to_owned(),
            registrations: Vec::new(),
            candidates: Vec::new(),
            log_path: None,
            artifact: None,
        };
        let results = service.register_reviewed(
            &outcome,
            &[],
            &[PathBuf::from("/prefix/drive_c/tools/helper.exe")],
            AppKind::Tool,
        )?;
        assert_eq!(results.len(), 1);
        let app = &results[0].entry;
        assert_eq!(app.slug, "helper", "the manual add is named by its stem");
        assert_eq!(app.kind, AppKind::Tool);
        assert_eq!(app.prefix, "default");
        Ok(())
    }

    #[test]
    fn review_dedupes_kept_and_manually_added_exes() -> anyhow::Result<()> {
        // The same exe via a shortcut and a manual add — or two shortcuts
        // — registers once.
        let mock = MockStorage::new(healthy_tree());
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
        let candidate = Candidate {
            exe: PathBuf::from("/prefix/drive_c/game.exe"),
            label: "game".to_owned(),
        };
        let outcome = InstallOutcome {
            prefix_slug: "default".to_owned(),
            registrations: Vec::new(),
            candidates: vec![candidate.clone()],
            log_path: None,
            artifact: None,
        };
        let results = service.register_reviewed(
            &outcome,
            &[candidate.clone(), candidate],
            &[PathBuf::from("/prefix/drive_c/game.exe")],
            AppKind::Game,
        )?;
        assert_eq!(results.len(), 1, "the exe registers exactly once");
        assert_eq!(service.storage.list_apps()?.len(), 1);
        Ok(())
    }

    #[test]
    fn review_reinstalling_a_registered_exe_updates_the_same_entry() -> anyhow::Result<()> {
        let mock = MockStorage::new(healthy_tree());
        mock.add_app(AppEntry {
            slug: "balatro".to_owned(),
            exe: PathBuf::from("/prefix/drive_c/balatro.exe"),
            kind: AppKind::Tool,
            prefix: "default".to_owned(),
            overrides: Overrides::default(),
            runner: None,
            source_installer: None,
            installed_at: None,
        });
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
        let candidate = Candidate {
            exe: PathBuf::from("/prefix/drive_c/balatro.exe"),
            label: "Balatro".to_owned(),
        };
        let outcome = InstallOutcome {
            prefix_slug: "games".to_owned(),
            registrations: Vec::new(),
            candidates: vec![candidate.clone()],
            log_path: None,
            artifact: Some(PathBuf::from("/tmp/setup.exe")),
        };
        let results = service.register_reviewed(&outcome, &[candidate], &[], AppKind::Game)?;
        assert_eq!(results.len(), 1);
        assert!(results[0].was_update, "identity stays with the exe path");
        assert_eq!(
            results[0].entry.slug, "balatro",
            "the slug stays the entry's"
        );
        assert_eq!(results[0].entry.kind, AppKind::Game);
        assert_eq!(
            results[0].entry.prefix, "games",
            "the session prefix rebinds"
        );
        assert_eq!(
            results[0].entry.source_installer.as_deref(),
            Some(Path::new("/tmp/setup.exe")),
            "the re-session's artifact refreshes the metadata"
        );
        assert_eq!(service.storage.list_apps()?.len(), 1, "no duplicate entry");
        Ok(())
    }

    #[test]
    fn review_preflights_every_exe_before_any_write() -> anyhow::Result<()> {
        // Two good candidates and one missing manual add: the whole review
        // aborts with nothing written — never a partial registration.
        let mock = MockStorage::new(healthy_tree());
        mock.mark_exe_missing(Path::new("/prefix/drive_c/gone.exe"));
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
        let keep = vec![
            Candidate {
                exe: PathBuf::from("/prefix/drive_c/a.exe"),
                label: "A".to_owned(),
            },
            Candidate {
                exe: PathBuf::from("/prefix/drive_c/b.exe"),
                label: "B".to_owned(),
            },
        ];
        let outcome = InstallOutcome {
            prefix_slug: "default".to_owned(),
            registrations: Vec::new(),
            candidates: keep.clone(),
            log_path: None,
            artifact: None,
        };
        let err = service
            .register_reviewed(
                &outcome,
                &keep,
                &[PathBuf::from("/prefix/drive_c/gone.exe")],
                AppKind::Game,
            )
            .expect_err("the missing add aborts the review");
        assert!(
            matches!(&err, InstallError::Storage(StorageError::NotFound(_))),
            "the missing exe is named: {err}"
        );
        assert!(
            service.storage.list_apps()?.is_empty(),
            "nothing was written by the aborted review"
        );
        Ok(())
    }

    #[test]
    fn review_rejects_unslugifiable_labels_without_writing() -> anyhow::Result<()> {
        let mock = MockStorage::new(healthy_tree());
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
        let outcome = InstallOutcome {
            prefix_slug: "default".to_owned(),
            registrations: Vec::new(),
            candidates: Vec::new(),
            log_path: None,
            artifact: None,
        };
        assert!(matches!(
            service.register_reviewed(
                &outcome,
                &[],
                &[PathBuf::from("/prefix/drive_c/!!!.exe")],
                AppKind::Game,
            ),
            Err(InstallError::Storage(StorageError::Invalid(_)))
        ));
        assert!(service.storage.list_apps()?.is_empty());
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn installer_branch_records_the_session_artifact() -> anyhow::Result<()> {
        // The session's artifact becomes the source_installer metadata of
        // entries registered from its review. The configured stub runner
        // plays wine; no real wine needed.
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cellar-app-artifact-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir)?;
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs)?;
        let wine = dir.join("stub-wine");
        write_stub_script(&wine, "exit 0\n")?;
        let mock = MockStorage::new(healthy_tree())
            .with_log_dir(logs)
            .with_prefix_base(dir.join("prefixes"));
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults {
                runner: Some(RunnerSpec::new(RunnerFamily::Wine)),
                ..PrefixDefaults::default()
            },
        });
        let service = InstallService::new(
            mock,
            StubResolver::new(Ok(wine_resolved_at(&wine))),
            StubDesktop::new(),
        );
        let outcome = service.install(
            Path::new("/tmp/setup.exe"),
            "default",
            None,
            AppKind::Game,
            ArtifactKind::Installer,
        )?;
        assert_eq!(
            outcome.artifact.as_deref(),
            Some(Path::new("/tmp/setup.exe")),
            "the canonical artifact is part of the session record"
        );
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
        let service = InstallService::new(
            mock,
            StubResolver::new(Ok(wine_resolved_at(&wine))),
            StubDesktop::new(),
        );
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
        let service = InstallService::new(
            mock,
            StubResolver::new(Ok(wine_resolved_at(&wine))),
            StubDesktop::new(),
        );
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
            let service = InstallService::new(mock, resolver, StubDesktop::new());
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
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
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
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
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
    fn list_joins_tree_health_for_entry_status() -> anyhow::Result<()> {
        let mut health = healthy_tree();
        health.missing_exes.push("balatro".to_owned());
        let mock = MockStorage::new(health);
        mock.add_app(entry("balatro", "/games/balatro.exe"));
        mock.add_app(entry("warpinator", "/prefix/drive_c/warpinator.exe"));
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
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
    fn uninstall_removes_only_that_entry() -> anyhow::Result<()> {
        let mock = MockStorage::new(healthy_tree());
        mock.add_app(entry("balatro", "/games/balatro.exe"));
        mock.add_app(entry("warpinator", "/prefix/drive_c/warpinator.exe"));
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
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
            Err(InstallError::Storage(StorageError::NotFound(_)))
        ));
        assert!(matches!(
            service.uninstall("../escape"),
            Err(InstallError::Storage(StorageError::Invalid(_)))
        ));
        Ok(())
    }

    #[test]
    fn registration_derives_the_launcher_entry_and_icon() -> anyhow::Result<()> {
        // AC: registering an app creates its launcher entry (and its
        // cached icon); the identity the adapter sees is the exe path.
        let desktop = StubDesktop::new();
        let service = InstallService::new(
            MockStorage::new(healthy_tree()),
            StubResolver::ok(),
            desktop.clone(),
        );
        service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Game,
            ArtifactKind::Standalone,
        )?;
        assert_eq!(
            desktop.created(),
            [("balatro".to_owned(), None)],
            "the registration creates the entry for the app's slug"
        );
        assert_eq!(
            desktop.icons(),
            [PathBuf::from("/games/balatro.exe")],
            "the icon is extracted from the exe"
        );
        Ok(())
    }

    #[test]
    fn uninstall_removes_the_launcher_entry() -> anyhow::Result<()> {
        // AC: uninstalling removes the launcher entry too.
        let desktop = StubDesktop::new();
        let service = InstallService::new(
            MockStorage::new(healthy_tree()),
            StubResolver::ok(),
            desktop.clone(),
        );
        service
            .storage
            .add_app(entry("balatro", "/games/balatro.exe"));
        service.uninstall("balatro")?;
        assert_eq!(
            desktop.removed(),
            ["balatro".to_owned()],
            "the uninstall removes the app's entry"
        );
        Ok(())
    }

    #[test]
    fn reinstalling_with_a_new_name_renames_the_entry() -> anyhow::Result<()> {
        // AC: renaming an app updates the entry file name — the identity
        // stays the exe path, so the refresh reports the previous slug.
        let desktop = StubDesktop::new();
        let service = InstallService::new(
            MockStorage::new(healthy_tree()),
            StubResolver::ok(),
            desktop.clone(),
        );
        service
            .storage
            .add_app(entry("balatro", "/games/balatro.exe"));
        service.install(
            Path::new("/games/balatro.exe"),
            "default",
            Some("Poker Night"),
            AppKind::Game,
            ArtifactKind::Standalone,
        )?;
        assert_eq!(
            desktop.created(),
            [("poker-night".to_owned(), Some("balatro".to_owned()))],
            "the re-registration renames: the entry refresh names the previous slug"
        );
        let apps = service.storage.list_apps()?;
        assert_eq!(apps.len(), 1, "identity stays — one entry");
        assert_eq!(
            apps[0].slug, "poker-night",
            "the file name follows the rename"
        );
        assert_eq!(
            apps[0].exe,
            PathBuf::from("/games/balatro.exe"),
            "the exe stays"
        );
        Ok(())
    }

    #[test]
    fn reinstalling_without_a_name_keeps_the_entry_name() -> anyhow::Result<()> {
        // US26: renaming requires an explicit name — the stem-derived
        // default of a re-install never renames an entry (its slug stays;
        // only kind and binding refresh).
        let desktop = StubDesktop::new();
        let service = InstallService::new(
            MockStorage::new(healthy_tree()),
            StubResolver::ok(),
            desktop.clone(),
        );
        service
            .storage
            .add_app(entry("balatro", "/games/balatro.exe"));
        service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Game,
            ArtifactKind::Standalone,
        )?;
        service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Tool,
            ArtifactKind::Standalone,
        )?;
        assert_eq!(
            desktop.created(),
            [("balatro".to_owned(), None), ("balatro".to_owned(), None)],
            "no explicit name, no rename — the entry and its file stay"
        );
        let apps = service.storage.list_apps()?;
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].slug, "balatro");
        assert_eq!(apps[0].kind, AppKind::Tool, "kind refreshes as usual");
        Ok(())
    }

    #[test]
    fn desktop_sync_re_derives_entries_icons_and_prunes_stale() -> anyhow::Result<()> {
        // AC: deleting the cache leaves entries functional after a
        // re-derivation — the sweep re-derives every entry from the
        // tree, prunes stale files, and is strictly one-way (it only
        // reads app state).
        let desktop = StubDesktop::new();
        let service = DesktopSync::new(MockStorage::new(healthy_tree()), desktop.clone());
        service
            .storage
            .add_app(entry("balatro", "/games/balatro.exe"));
        service
            .storage
            .add_app(entry("warpinator", "/games/warpinator.exe"));
        let report = service.sync()?;
        assert_eq!(report.entries, 2);
        assert_eq!(report.icons, 0, "the stub finds no icons");
        let created: Vec<_> = desktop
            .created()
            .into_iter()
            .map(|(slug, _)| slug)
            .collect();
        assert_eq!(created, ["balatro".to_owned(), "warpinator".to_owned()]);
        assert_eq!(
            desktop.prunes(),
            [vec!["balatro".to_owned(), "warpinator".to_owned()]],
            "the stale sweep keeps exactly the tree's apps"
        );
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

    /// One recorded create-entry request: the app's slug plus the previous
    /// slug it was renamed from, if any.
    type EntryRequest = (String, Option<String>);

    /// A desktop-integrator double: records the entry/icon/prune calls
    /// instead of touching a filesystem — the storage mock's twin on the
    /// desktop seam. `Arc`-shared so tests can inspect the records after
    /// the stub has been moved into the service.
    #[derive(Debug, Clone, Default)]
    struct StubDesktop {
        /// Every `(slug, previous_slug)` pair a registration asked for.
        created: Arc<Mutex<Vec<EntryRequest>>>,
        /// Every slug a removal was asked for.
        removed: Arc<Mutex<Vec<String>>>,
        /// Every icon extraction request (the exe path).
        icons: Arc<Mutex<Vec<PathBuf>>>,
        /// Every stale-sweep keep-list.
        prunes: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl StubDesktop {
        fn new() -> Self {
            Self::default()
        }

        fn created(&self) -> Vec<EntryRequest> {
            self.created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn removed(&self) -> Vec<String> {
            self.removed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn icons(&self) -> Vec<PathBuf> {
            self.icons
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        fn prunes(&self) -> Vec<Vec<String>> {
            self.prunes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl __sealed::Sealed for StubDesktop {}

    impl DesktopIntegrator for StubDesktop {
        fn create_entry(
            &self,
            app: &AppEntry,
            previous_slug: Option<&str>,
            _icon: Option<&Path>,
        ) -> Result<PathBuf, DesktopError> {
            self.created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((app.slug.clone(), previous_slug.map(str::to_owned)));
            Ok(PathBuf::from(format!("cellar-{}.desktop", app.slug)))
        }

        fn remove_entry(&self, app: &AppEntry) -> Result<(), DesktopError> {
            self.removed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(app.slug.clone());
            Ok(())
        }

        fn install_icon(&self, exe: &Path) -> Result<Option<PathBuf>, DesktopError> {
            self.icons
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(exe.to_path_buf());
            Ok(None)
        }

        fn prune_entries(&self, keep: &[&str]) -> Result<Vec<PathBuf>, DesktopError> {
            self.prunes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(keep.iter().map(|slug| (*slug).to_owned()).collect());
            Ok(Vec::new())
        }

        fn set_file_association(&self) -> Result<(), DesktopError> {
            Ok(())
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
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
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
        let service = InstallService::new(mock, StubResolver::ok(), StubDesktop::new());
        let listed = service.list()?;
        assert_eq!(
            listed[0].prefix_runner, None,
            "a broken prefix degrades the column to the floor — the doctor flags it"
        );
        Ok(())
    }
}
