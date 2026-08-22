//! Application use-cases for this slice (#26): the prefix lifecycle and the
//! tree-health doctor check. Thin orchestration over the `Storage` port —
//! concrete adapters are injected only at the composition root.

use cellar_core::Prefix;
use cellar_core::errors::StorageError;
use cellar_core::health::TreeHealth;
use cellar_core::ports::Storage;
use cellar_core::slug;

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
    use super::{DoctorService, PrefixService, Storage};

    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use cellar_core::entities::{AppEntry, Candidate, Settings};
    use cellar_core::errors::StorageError;
    use cellar_core::health::TreeHealth;
    use cellar_core::manifest::RunnerManifest;
    use cellar_core::{Prefix, PrefixDefaults};

    /// In-memory `Storage` double: records the calls the services make and
    /// answers with canned state. `Mutex` interior so the double meets the
    /// port's `Send + Sync` bound.
    #[derive(Debug)]
    struct MockStorage {
        created: Mutex<Vec<String>>,
        deleted: Mutex<Vec<String>>,
        health: TreeHealth,
    }

    impl MockStorage {
        fn new(health: TreeHealth) -> Self {
            Self {
                created: Mutex::new(Vec::new()),
                deleted: Mutex::new(Vec::new()),
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
            self.created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(slug.to_owned());
            Ok(Prefix {
                slug: slug.to_owned(),
                defaults: PrefixDefaults::default(),
            })
        }

        fn list_prefixes(&self) -> Result<Vec<Prefix>, StorageError> {
            Ok(Vec::new())
        }

        fn load_prefix(&self, slug: &str) -> Result<Prefix, StorageError> {
            Ok(Prefix {
                slug: slug.to_owned(),
                defaults: PrefixDefaults::default(),
            })
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
            Ok(Vec::new())
        }

        fn load_app(&self, _slug: &str) -> Result<AppEntry, StorageError> {
            Err(StorageError::NotFound("/mock/apps".to_owned()))
        }

        fn save_app(&self, _app: &AppEntry) -> Result<(), StorageError> {
            Ok(())
        }

        fn delete_app(&self, _slug: &str) -> Result<(), StorageError> {
            Ok(())
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
}
