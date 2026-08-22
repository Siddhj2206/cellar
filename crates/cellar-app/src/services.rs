//! Application use-cases: the `AppEntry` registry — standalone install
//! (registered without executing), app list with per-entry status, uninstall
//! degrading to entry removal (#27) — plus the prefix lifecycle and the
//! tree-health doctor check (#26). Thin orchestration over the `Storage`
//! port — concrete adapters are injected only at the composition root.

use std::collections::BTreeSet;
use std::path::Path;

use cellar_core::Prefix;
use cellar_core::entities::{AppEntry, AppKind, Overrides};
use cellar_core::errors::StorageError;
use cellar_core::health::TreeHealth;
use cellar_core::ports::Storage;
use cellar_core::slug;

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

/// One row of `cellar list`: a registered entry plus its current status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedEntry {
    pub entry: AppEntry,
    pub status: EntryStatus,
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
    /// (blueprint §8: slug, kind, prefix, runner, status). Invalid
    /// hand-edited entries are skipped — the doctor flags them (ADR 0001).
    pub fn list(&self) -> Result<Vec<ListedEntry>, StorageError> {
        let entries = self.storage.list_apps()?;
        let missing: BTreeSet<String> = self
            .storage
            .tree_health()?
            .missing_exes
            .into_iter()
            .collect();
        Ok(entries
            .into_iter()
            .map(|entry| ListedEntry {
                status: if missing.contains(&entry.slug) {
                    EntryStatus::ExeMissing
                } else {
                    EntryStatus::Ok
                },
                entry,
            })
            .collect())
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
    use super::{DoctorService, InstallService, PrefixService, Storage};

    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use cellar_core::entities::{AppEntry, AppKind, Candidate, Overrides, Settings};
    use cellar_core::errors::StorageError;
    use cellar_core::health::TreeHealth;
    use cellar_core::manifest::RunnerManifest;
    use cellar_core::{Prefix, PrefixDefaults};

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
    }

    impl cellar_core::ports::__sealed::Sealed for MockStorage {}

    impl Storage for MockStorage {
        fn data_root(&self) -> &Path {
            Path::new("/mock")
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

        fn load_app(&self, _slug: &str) -> Result<AppEntry, StorageError> {
            Err(StorageError::NotFound("/mock/apps".to_owned()))
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
}
