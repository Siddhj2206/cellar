//! Application use-cases: the `AppEntry` registry — standalone install
//! (registered without executing), app list with per-entry status, uninstall
//! degrading to entry removal (#27) — the prefix lifecycle and the
//! tree-health doctor check (#26), and the `LaunchApp` use-case (#28):
//! resolve → check → plan with nothing spawning. Thin orchestration over
//! the `Storage` port — concrete adapters are injected only at the
//! composition root.

use std::collections::BTreeSet;
use std::path::Path;

use cellar_core::Prefix;
use cellar_core::entities::{AppEntry, AppKind, Overrides};
use cellar_core::errors::StorageError;
use cellar_core::health::TreeHealth;
use cellar_core::ports::{RunnerResolver, Storage};
use cellar_core::slug;
use cellar_core::types::{LaunchPlan, RunnerSpec};
use cellar_launch::{LaunchError, build_plan, select_spec};

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

/// The result of a standalone registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallResult {
    pub entry: AppEntry,
    /// Whether the registration updated an existing entry with the same exe
    /// (identity = canonical exe path) instead of creating a new one.
    pub was_update: bool,
}

/// The `AppEntry` registry: standalone install, list with status, uninstall.
/// Identity is the canonical exe path — re-installing the same exe updates
/// the same entry (blueprint §6, §8); the slug is the display/file name with
/// `-2` dedupe.
pub struct InstallService<S: Storage> {
    storage: S,
}

impl<S: Storage> InstallService<S> {
    /// The service over one storage adapter.
    pub fn new(storage: S) -> Self {
        Self { storage }
    }

    /// Register a standalone Windows exe without executing anything
    /// (blueprint §8: the standalone branch of the flagship flow). Binds the
    /// entry to `prefix`, picking or creating it. Re-installing the same exe
    /// updates the same entry — its slug and identity stay, its kind and
    /// prefix binding take the new flags.
    pub fn install(
        &self,
        exe: &Path,
        prefix: &str,
        name: Option<&str>,
        kind: AppKind,
    ) -> Result<InstallResult, StorageError> {
        let canonical = self.storage.canonicalize_exe(exe)?;
        if !is_exe(&canonical) {
            return Err(StorageError::Invalid(format!(
                "{}: not a Windows executable — only .exe files are registered standalone",
                canonical.display()
            )));
        }
        if !slug::is_valid_slug(prefix) {
            return Err(StorageError::Invalid(format!(
                "invalid prefix slug {prefix:?}"
            )));
        }
        // Determine the display name up front — an unslugifiable name is
        // rejected before any side effect. On a re-install the entry keeps
        // its slug, so an unusable `name` is a user error to surface, never
        // silently ignored.
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
            )));
        }
        // Pick or create the bound prefix (blueprint §8 step 1, default
        // `default`). A missing or broken hand-edited prefix yields a fresh
        // sibling via the storage dedupe — never a clobber (ADR 0001).
        let prefix_slug = match self.storage.load_prefix(prefix) {
            Ok(_) => prefix.to_owned(),
            Err(StorageError::NotFound(_) | StorageError::Invalid(_)) => {
                self.storage.create_prefix(prefix)?.slug
            }
            Err(other) => return Err(other),
        };
        // Identity is the canonical exe path: re-install finds the existing
        // entry and updates it in place.
        if let Some(mut existing) = self
            .storage
            .list_apps()?
            .into_iter()
            .find(|app| app.exe == canonical)
        {
            existing.kind = kind;
            existing.prefix = prefix_slug;
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
            slug: slug::dedupe_slug(&base, &taken),
            exe: canonical,
            kind,
            prefix: prefix_slug,
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

/// The `LaunchApp` use-case (blueprint §7): resolve → check → plan, with
/// nothing spawning until the plan is frozen. This slice delivers the
/// frozen plan as a pure, printable value — `--dry-run` and the GUI preview
/// render it; spawning it lands with the execute slice (#29).
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
        let settings = self.storage.load_settings().map_err(LaunchError::Storage)?;
        // Resolve stage, resolution: app override → prefix default → floor
        // picks the spec; the resolver runs the family's order (configured
        // → managed → PATH, research #18).
        let spec = select_spec(&entry, &prefix, &settings);
        let resolved = self.resolver.resolve(&spec).map_err(LaunchError::Resolve)?;
        // Check stage: exactly this launch's dependencies — the registered
        // exe must still be a regular file.
        self.storage
            .canonicalize_exe(&entry.exe)
            .map_err(|err| match err {
                StorageError::NotFound(_) | StorageError::Invalid(_) => LaunchError::ExeMissing {
                    slug: entry.slug.clone(),
                    exe: entry.exe.clone(),
                },
                other => LaunchError::Storage(other),
            })?;
        // Plan stage: the pure, printable plan. The wrapper chain is empty
        // this slice (no wrapper activation rules yet, #34).
        let prefix_dir = self.storage.prefix_dir(&prefix_slug);
        build_plan(&entry, &prefix, &resolved, &prefix_dir, &[], args)
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
    use super::{DoctorService, InstallService, LaunchApp, PrefixService, Storage};

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
    use cellar_launch::LaunchError;

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
                health,
            }
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
            PathBuf::from("/mock/prefixes").join(slug)
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
            Err(StorageError::Unimplemented("mock".to_owned()))
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

    #[test]
    fn install_registers_a_new_entry_without_executing() -> Result<(), StorageError> {
        let mock = MockStorage::new(healthy_tree());
        let service = InstallService::new(mock);
        let result = service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Game,
        )?;
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
        Ok(())
    }

    #[test]
    fn install_uses_the_flag_name_and_dedupes_the_slug() -> Result<(), StorageError> {
        let mock = MockStorage::new(healthy_tree());
        mock.take_app_slugs(&["my-game", "my-game-2"]);
        let service = InstallService::new(mock);
        let result = service.install(
            Path::new("/games/game.exe"),
            "default",
            Some("My Game"),
            AppKind::Game,
        )?;
        assert_eq!(
            result.entry.slug, "my-game-3",
            "-2 dedupe against the slug domain"
        );
        Ok(())
    }

    #[test]
    fn reinstalling_the_same_exe_updates_the_same_entry() -> Result<(), StorageError> {
        let mock = MockStorage::new(healthy_tree());
        let service = InstallService::new(mock);
        let first = service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Game,
        )?;
        assert!(!first.was_update);
        let second = service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Tool,
        )?;
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
    fn reinstalling_rebinds_the_prefix() -> Result<(), StorageError> {
        let mock = MockStorage::new(healthy_tree());
        let service = InstallService::new(mock);
        service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Game,
        )?;
        let rebound = service.install(
            Path::new("/games/balatro.exe"),
            "games",
            None,
            AppKind::Game,
        )?;
        assert!(rebound.was_update);
        assert_eq!(rebound.entry.prefix, "games");
        assert_eq!(service.storage.list_apps()?.len(), 1);
        Ok(())
    }

    #[test]
    fn install_uses_an_existing_prefix_without_creating_it() -> Result<(), StorageError> {
        let mock = MockStorage::new(healthy_tree());
        mock.add_prefix(Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults::default(),
        });
        let service = InstallService::new(mock);
        service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Game,
        )?;
        assert!(service.storage.created().is_empty());
        Ok(())
    }

    #[test]
    fn install_sidesteps_a_broken_hand_edited_prefix() -> Result<(), StorageError> {
        // A broken prefix.toml is never clobbered (ADR 0001): the install
        // binds to a freshly deduped sibling — the same dedupe `prefix
        // create` applies, so the requested slug is never silently reused.
        let mock = MockStorage::new(healthy_tree());
        mock.mark_broken_prefix("default");
        let service = InstallService::new(mock);
        let result = service.install(
            Path::new("/games/balatro.exe"),
            "default",
            None,
            AppKind::Game,
        )?;
        assert_eq!(service.storage.created(), ["default"]);
        assert_eq!(result.entry.prefix, "default-2", "the fresh sibling binds");
        Ok(())
    }

    #[test]
    fn install_rejects_bad_inputs_before_touching_the_tree() -> Result<(), StorageError> {
        let mock = MockStorage::new(healthy_tree());
        let service = InstallService::new(mock);
        // A non-exe is not a standalone artifact.
        assert!(matches!(
            service.install(
                Path::new("/games/readme.txt"),
                "default",
                None,
                AppKind::Game
            ),
            Err(StorageError::Invalid(_))
        ));
        // An escaping prefix slug is rejected up front.
        assert!(matches!(
            service.install(
                Path::new("/games/balatro.exe"),
                "../escape",
                None,
                AppKind::Game
            ),
            Err(StorageError::Invalid(_))
        ));
        // An unslugifiable display name cannot name an entry.
        assert!(matches!(
            service.install(
                Path::new("/games/balatro.exe"),
                "default",
                Some("!!!"),
                AppKind::Game
            ),
            Err(StorageError::Invalid(_))
        ));
        assert!(
            service.storage.list_apps()?.is_empty(),
            "nothing registered"
        );
        assert!(service.storage.created().is_empty(), "no prefix created");
        Ok(())
    }

    #[test]
    fn list_joins_tree_health_for_entry_status() -> Result<(), StorageError> {
        let mut health = healthy_tree();
        health.missing_exes.push("balatro".to_owned());
        let mock = MockStorage::new(health);
        mock.add_app(entry("balatro", "/games/balatro.exe"));
        mock.add_app(entry("warpinator", "/prefix/drive_c/warpinator.exe"));
        let service = InstallService::new(mock);
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
        let service = InstallService::new(mock);
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
        let service = InstallService::new(mock);
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
        let service = InstallService::new(mock);
        let listed = service.list()?;
        assert_eq!(
            listed[0].prefix_runner, None,
            "a broken prefix degrades the column to the floor — the doctor flags it"
        );
        Ok(())
    }
}
