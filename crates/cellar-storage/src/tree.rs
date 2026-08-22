//! The file-tree adapter (blueprint §6, ADR 0001): TOML mapping between the
//! `core` entities and the locked tree, implementing `core::ports::Storage` —
//! the sole writer of the tree.
//!
//! File shape: `schema_version` on the first line, entity fields below in
//! plain TOML. Reads parse the file twice — once for the version header, once
//! for the entity — so files stay hand-editable and unknown future fields are
//! ignored by serde rather than rejected.
//!
//! Writes are atomic: a unique temp file in the same directory, `sync_all`,
//! then `rename` over the target. A reader or concurrent writer can never
//! observe a torn file, and the app service is the only writer (ADR 0001).
//!
//! Every operation except [`Storage::tree_health`] self-initializes the tree:
//! the first run of any command creates the root, the four top-level
//! directories, and a default `settings.toml` — the "first run" contract of
//! blueprint §6. [`Storage::tree_health`] deliberately has no side effects:
//! the doctor reports exactly what is on disk.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cellar_core::entities::{AppEntry, Candidate, Prefix, PrefixDefaults, Settings};
use cellar_core::errors::StorageError;
use cellar_core::health::TreeHealth;
use cellar_core::manifest::RunnerManifest;
use cellar_core::ports::Storage;
use cellar_core::slug;

/// The schema version every written file carries (ADR 0001: migrations are
/// file-tree transforms — a version bump is a transform, not a rewrite).
pub const SCHEMA_VERSION: u32 = 1;

/// The four top-level directories of the tree (blueprint §6).
const TREE_DIRS: [&str; 4] = ["prefixes", "apps", "runtime", "cache"];

/// Serialization helper for the version header carried by every file.
#[derive(Serialize, Deserialize)]
struct FileMeta {
    schema_version: u32,
}

/// Monotonic counter making temp-file names unique within a process.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// The concrete `Storage` adapter: one root directory, TOML files, atomic
/// writes. Construct via [`TreeStore::from_env`] at the composition root, or
/// [`TreeStore::new`] with an explicit root (tests, embedded use).
#[derive(Debug, Clone)]
pub struct TreeStore {
    root: PathBuf,
}

impl TreeStore {
    /// The tree root per XDG: `$XDG_DATA_HOME/cellar`, falling back to
    /// `$HOME/.local/share/cellar` when `XDG_DATA_HOME` is unset or empty
    /// (ADR 0001).
    pub fn from_env() -> Result<Self, StorageError> {
        let base = match std::env::var("XDG_DATA_HOME") {
            Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => match std::env::var("HOME") {
                Ok(home) if !home.is_empty() => PathBuf::from(home).join(".local/share"),
                _ => {
                    return Err(StorageError::Io(
                        "neither XDG_DATA_HOME nor HOME is set".to_owned(),
                    ));
                }
            },
        };
        Ok(Self::new(base.join("cellar")))
    }

    /// A store rooted at `root` (tests and explicit deployments).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The on-disk directory of one prefix — the tree layout is the
    /// adapter's knowledge, never presentation's.
    pub fn prefix_dir(&self, slug: &str) -> PathBuf {
        self.root.join("prefixes").join(slug)
    }

    fn settings_path(&self) -> PathBuf {
        self.root.join("settings.toml")
    }

    fn prefix_file(&self, slug: &str) -> PathBuf {
        self.prefix_dir(slug).join("prefix.toml")
    }

    fn app_file(&self, slug: &str) -> PathBuf {
        self.root.join("apps").join(format!("{slug}.toml"))
    }

    /// First run (blueprint §6): create the root, the four directories, and a
    /// default `settings.toml` when absent. Idempotent; never overwrites an
    /// existing file — hand-edited state stays authoritative.
    fn ensure_tree(&self) -> Result<(), StorageError> {
        fs::create_dir_all(&self.root).map_err(|e| io_err(&self.root, &e))?;
        for dir in TREE_DIRS {
            let path = self.root.join(dir);
            fs::create_dir_all(&path).map_err(|e| io_err(&path, &e))?;
        }
        if !self.settings_path().exists() {
            Self::write_envelope(&self.settings_path(), &Settings::default())?;
        }
        Ok(())
    }

    /// Read a file as an entity: TOML parse (unknown fields ignored — future
    /// fields must not break older binaries), then schema check. A malformed
    /// or version-mismatched file is `Invalid` — hand-edit damage degrades to
    /// a skipped, doctor-flagged entry, never a rewrite (ADR 0001).
    fn read_envelope<T: DeserializeOwned>(path: &Path) -> Result<T, StorageError> {
        let text = fs::read_to_string(path).map_err(|e| io_err(path, &e))?;
        let meta: FileMeta =
            toml::from_str(&text).map_err(|_| StorageError::Invalid(path.display().to_string()))?;
        if meta.schema_version != SCHEMA_VERSION {
            return Err(StorageError::Invalid(format!(
                "{}: schema version {} (the tree reads {SCHEMA_VERSION})",
                path.display(),
                meta.schema_version
            )));
        }
        toml::from_str(&text).map_err(|_| StorageError::Invalid(path.display().to_string()))
    }

    /// Write an entity atomically: `schema_version` header + TOML body via a
    /// unique temp file in the same directory, then `rename` over the target.
    /// A reader or concurrent writer never observes a partial file.
    fn write_envelope<T: Serialize>(path: &Path, data: &T) -> Result<(), StorageError> {
        let dir = path.parent().ok_or_else(|| {
            StorageError::Io(format!("{} has no parent directory", path.display()))
        })?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let tmp = dir.join(format!(
            ".{name}.tmp.{}.{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let body = toml::to_string(data).map_err(|e| {
            StorageError::Invalid(format!("cannot serialize {}: {e}", path.display()))
        })?;
        let contents = format!("schema_version = {SCHEMA_VERSION}\n\n{body}");
        let result = (|| -> std::io::Result<()> {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
            fs::rename(&tmp, path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.map_err(|e| io_err(path, &e))
    }

    /// The prefix entry inside one directory, or `Invalid` when the file
    /// disagrees with its location (hand-edited slug drift) or carries an
    /// invalid slug. Walks use this, so a debris entry is skipped and
    /// doctor-flagged consistently everywhere.
    fn read_prefix_at(dir: &Path) -> Result<Prefix, StorageError> {
        let prefix: Prefix = Self::read_envelope(&dir.join("prefix.toml"))?;
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !slug::is_valid_slug(&prefix.slug) || prefix.slug != name {
            return Err(StorageError::Invalid(format!(
                "{}: file slug {:#?} does not match its directory {name:?}",
                dir.join("prefix.toml").display(),
                prefix.slug
            )));
        }
        Ok(prefix)
    }

    /// The app entry inside one file, or `Invalid` when the file disagrees
    /// with its name (hand-edited slug drift) or carries an invalid slug.
    fn read_app_at(path: &Path) -> Result<AppEntry, StorageError> {
        let app: AppEntry = Self::read_envelope(path)?;
        let name = path
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !slug::is_valid_slug(&app.slug) || app.slug != name {
            return Err(StorageError::Invalid(format!(
                "{}: file slug {:#?} does not match its file name {name:?}",
                path.display(),
                app.slug
            )));
        }
        Ok(app)
    }

    /// The names of every prefix directory (valid or not) — the dedupe
    /// domain, so a broken hand-edited prefix can never be silently
    /// overwritten by a fresh create.
    fn prefix_dir_slugs(&self) -> Result<BTreeSet<String>, StorageError> {
        let mut slugs = BTreeSet::new();
        for path in read_dir_sorted(&self.root.join("prefixes"))? {
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    slugs.insert(name.to_owned());
                }
            }
        }
        Ok(slugs)
    }

    /// Reject a slug that could escape the tree or address nothing, before
    /// any filesystem work. Every storage entry point validates — defence in
    /// depth on top of the app services' validation.
    fn require_valid_slug(kind: &str, slug: &str) -> Result<(), StorageError> {
        if slug::is_valid_slug(slug) {
            Ok(())
        } else {
            Err(StorageError::Invalid(format!(
                "invalid {kind} slug {slug:?}"
            )))
        }
    }

    /// The prefix file inside one already-existing directory, written without
    /// touching the tree or the directory itself.
    fn write_prefix_file(&self, prefix: &Prefix) -> Result<(), StorageError> {
        Self::write_envelope(&self.prefix_file(&prefix.slug), prefix)
    }

    /// Classify a read failure for the health report: `Invalid` files are
    /// flagged, `NotFound` is a race on disk and ignored, real I/O errors
    /// propagate.
    fn flag_invalid<T>(
        result: Result<T, StorageError>,
        relative: PathBuf,
        health: &mut TreeHealth,
    ) -> Result<(), StorageError> {
        match result {
            Ok(_) | Err(StorageError::NotFound(_)) => Ok(()),
            Err(StorageError::Invalid(_)) => {
                health.invalid_files.push(relative);
                Ok(())
            }
            Err(other) => Err(other),
        }
    }
}

impl cellar_core::ports::__sealed::Sealed for TreeStore {}

impl Storage for TreeStore {
    fn data_root(&self) -> &Path {
        &self.root
    }

    fn load_settings(&self) -> Result<Settings, StorageError> {
        self.ensure_tree()?;
        Self::read_envelope(&self.settings_path())
    }

    fn save_settings(&self, settings: &Settings) -> Result<(), StorageError> {
        self.ensure_tree()?;
        Self::write_envelope(&self.settings_path(), settings)
    }

    fn create_prefix(&self, slug: &str) -> Result<Prefix, StorageError> {
        Self::require_valid_slug("prefix", slug)?;
        self.ensure_tree()?;
        // The directory creation *is* the claim: `create_dir` fails when the
        // slug was taken since the snapshot, so two concurrent creates of the
        // same name dedupe atomically instead of racing (ADR 0001).
        loop {
            let final_slug = slug::dedupe_slug(slug, &self.prefix_dir_slugs()?);
            let dir = self.prefix_dir(&final_slug);
            match fs::create_dir(&dir) {
                Ok(()) => {
                    let prefix = Prefix {
                        slug: final_slug,
                        defaults: PrefixDefaults::default(),
                    };
                    self.write_prefix_file(&prefix)?;
                    return Ok(prefix);
                }
                // Lost the claim race — retry with the fresh directory state.
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(io_err(&dir, &err)),
            }
        }
    }

    fn list_prefixes(&self) -> Result<Vec<Prefix>, StorageError> {
        self.ensure_tree()?;
        let mut prefixes = Vec::new();
        for path in read_dir_sorted(&self.root.join("prefixes"))? {
            if !path.is_dir() {
                continue;
            }
            // Invalid entries are skipped here and flagged by `tree_health` —
            // never rewritten (ADR 0001).
            if let Ok(prefix) = Self::read_prefix_at(&path) {
                prefixes.push(prefix);
            }
        }
        Ok(prefixes)
    }

    fn load_prefix(&self, slug: &str) -> Result<Prefix, StorageError> {
        Self::require_valid_slug("prefix", slug)?;
        self.ensure_tree()?;
        let prefix: Prefix = Self::read_envelope(&self.prefix_file(slug))?;
        // Same consistency rule as the walks: a hand-edited slug drift is
        // invalid everywhere, not silently loadable.
        if prefix.slug != slug {
            return Err(StorageError::Invalid(format!(
                "{}: file slug {:#?} does not match the requested slug {slug:?}",
                self.prefix_file(slug).display(),
                prefix.slug
            )));
        }
        Ok(prefix)
    }

    fn save_prefix(&self, prefix: &Prefix) -> Result<(), StorageError> {
        Self::require_valid_slug("prefix", &prefix.slug)?;
        self.ensure_tree()?;
        let dir = self.prefix_dir(&prefix.slug);
        fs::create_dir_all(&dir).map_err(|e| io_err(&dir, &e))?;
        self.write_prefix_file(prefix)
    }

    fn delete_prefix(&self, slug: &str) -> Result<(), StorageError> {
        Self::require_valid_slug("prefix", slug)?;
        self.ensure_tree()?;
        let dir = self.prefix_dir(slug);
        if !dir.exists() {
            return Err(StorageError::NotFound(dir.display().to_string()));
        }
        // Ownership (ADR 0001): delete removes exactly this directory —
        // never more.
        fs::remove_dir_all(&dir).map_err(|e| io_err(&dir, &e))
    }

    fn list_apps(&self) -> Result<Vec<AppEntry>, StorageError> {
        self.ensure_tree()?;
        let mut apps = Vec::new();
        for path in read_dir_sorted(&self.root.join("apps"))? {
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            if let Ok(app) = Self::read_app_at(&path) {
                apps.push(app);
            }
        }
        Ok(apps)
    }

    fn load_app(&self, slug: &str) -> Result<AppEntry, StorageError> {
        Self::require_valid_slug("app", slug)?;
        self.ensure_tree()?;
        Self::read_envelope(&self.app_file(slug))
    }

    fn save_app(&self, app: &AppEntry) -> Result<(), StorageError> {
        Self::require_valid_slug("app", &app.slug)?;
        self.ensure_tree()?;
        Self::write_envelope(&self.app_file(&app.slug), app)
    }

    fn delete_app(&self, slug: &str) -> Result<(), StorageError> {
        Self::require_valid_slug("app", slug)?;
        self.ensure_tree()?;
        let file = self.app_file(slug);
        if !file.exists() {
            return Err(StorageError::NotFound(file.display().to_string()));
        }
        fs::remove_file(&file).map_err(|e| io_err(&file, &e))
    }

    fn tree_health(&self) -> Result<TreeHealth, StorageError> {
        let mut health = TreeHealth {
            root: self.root.clone(),
            tree_exists: true,
            missing_dirs: Vec::new(),
            missing_files: Vec::new(),
            invalid_files: Vec::new(),
            orphan_prefix_dirs: Vec::new(),
            schema_version: SCHEMA_VERSION,
        };
        if !self.root.exists() {
            health.tree_exists = false;
            health.missing_dirs = TREE_DIRS.iter().map(PathBuf::from).collect();
            return Ok(health);
        }
        for dir in TREE_DIRS {
            if !self.root.join(dir).exists() {
                health.missing_dirs.push(PathBuf::from(dir));
            }
        }
        if self.settings_path().exists() {
            Self::flag_invalid(
                Self::read_envelope::<Settings>(&self.settings_path()),
                PathBuf::from("settings.toml"),
                &mut health,
            )?;
        } else {
            health.missing_files.push(PathBuf::from("settings.toml"));
        }
        let prefixes_dir = self.root.join("prefixes");
        if prefixes_dir.exists() {
            for path in read_dir_sorted(&prefixes_dir)? {
                if !path.is_dir() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let relative = PathBuf::from("prefixes").join(name);
                if !slug::is_valid_slug(name) {
                    // Unreachable debris: the entry can never be addressed.
                    health.orphan_prefix_dirs.push(relative);
                    continue;
                }
                let file = relative.join("prefix.toml");
                if path.join("prefix.toml").exists() {
                    Self::flag_invalid(Self::read_prefix_at(&path), file, &mut health)?;
                } else {
                    health.orphan_prefix_dirs.push(relative);
                }
            }
        }
        let apps_dir = self.root.join("apps");
        if apps_dir.exists() {
            for path in read_dir_sorted(&apps_dir)? {
                if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                let relative = PathBuf::from("apps").join(
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default(),
                );
                Self::flag_invalid(Self::read_app_at(&path), relative, &mut health)?;
            }
        }
        Ok(health)
    }

    fn discover_executables(&self, _prefix: &Prefix) -> Result<Vec<Candidate>, StorageError> {
        Err(StorageError::Unimplemented(
            ".lnk discovery lands with the AppEntry slice (#27)".to_owned(),
        ))
    }

    fn install_managed(&self, _manifest: &RunnerManifest) -> Result<PathBuf, StorageError> {
        Err(StorageError::Unimplemented(
            "the shared installer pipeline lands with the managed-runtime slice (#34)".to_owned(),
        ))
    }
}

/// Map a filesystem error at `path` onto the storage taxonomy: `NotFound`
/// for missing nodes, `Io` otherwise.
fn io_err(path: &Path, error: &std::io::Error) -> StorageError {
    if error.kind() == std::io::ErrorKind::NotFound {
        StorageError::NotFound(path.display().to_string())
    } else {
        StorageError::Io(format!("{}: {error}", path.display()))
    }
}

/// Sorted directory entries — deterministic walks for stable output and
/// tests. Entry-level read errors are skipped; the walk itself reports
/// failures loudly.
fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, StorageError> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| io_err(dir, &e))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    use cellar_core::entities::{AppKind, Overrides};

    /// A fresh, unique root for one test; removed afterwards.
    fn temp_root(tag: &str) -> PathBuf {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("cellar-test-{tag}-{}-{seq}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn store(tag: &str) -> (TreeStore, PathBuf) {
        let root = temp_root(tag);
        (TreeStore::new(root.clone()), root)
    }

    fn file_text(root: &Path, relative: &str) -> String {
        fs::read_to_string(root.join(relative)).unwrap_or_else(|e| panic!("read {relative}: {e}"))
    }

    #[test]
    fn first_run_creates_the_locked_tree() -> Result<(), StorageError> {
        let (store, root) = store("first-run");
        store.create_prefix("default")?;
        for dir in TREE_DIRS {
            assert!(root.join(dir).is_dir(), "{dir} directory missing");
        }
        let settings = file_text(&root, "settings.toml");
        assert!(settings.starts_with("schema_version = 1"));
        Ok(())
    }

    #[test]
    fn every_written_file_carries_schema_version() -> Result<(), StorageError> {
        let (store, root) = store("schema-version");
        store.create_prefix("games")?;
        assert!(file_text(&root, "settings.toml").starts_with("schema_version = 1"));
        assert!(
            file_text(&root, "prefixes/games/prefix.toml").starts_with("schema_version = 1"),
            "prefix.toml lacks the schema header"
        );
        Ok(())
    }

    #[test]
    fn create_writes_defaults_and_dedupes_with_dash_two() -> Result<(), StorageError> {
        let (store, _) = store("dedupe");
        assert_eq!(store.create_prefix("games")?.slug, "games");
        assert_eq!(store.create_prefix("games")?.slug, "games-2");
        assert_eq!(store.create_prefix("games")?.slug, "games-3");
        let slugs: Vec<_> = store
            .list_prefixes()?
            .iter()
            .map(|p| p.slug.clone())
            .collect();
        assert_eq!(slugs, ["games", "games-2", "games-3"]);
        Ok(())
    }

    #[test]
    fn load_returns_the_saved_prefix_and_updates_round_trip() -> Result<(), StorageError> {
        let (store, _) = store("round-trip");
        store.create_prefix("games")?;
        let mut prefix = store.load_prefix("games")?;
        prefix
            .defaults
            .env
            .insert("DXVK_HUD".to_owned(), "fps".to_owned());
        store.save_prefix(&prefix)?;
        let loaded = store.load_prefix("games")?;
        assert_eq!(
            loaded.defaults.env.get("DXVK_HUD").map(String::as_str),
            Some("fps")
        );
        Ok(())
    }

    #[test]
    fn delete_removes_exactly_its_directory() -> Result<(), StorageError> {
        let (store, root) = store("delete");
        store.create_prefix("alpha")?;
        store.create_prefix("beta")?;
        store.delete_prefix("alpha")?;
        assert!(
            !root.join("prefixes/alpha").exists(),
            "alpha dir not removed"
        );
        assert!(root.join("prefixes/beta").exists(), "beta dir removed");
        assert_eq!(store.list_prefixes()?.len(), 1);
        assert!(matches!(
            store.delete_prefix("alpha"),
            Err(StorageError::NotFound(_))
        ));
        Ok(())
    }

    #[test]
    fn hand_edited_valid_file_is_read_back() -> Result<(), StorageError> {
        let (store, root) = store("hand-edit");
        store.create_prefix("games")?;
        let prefix_file = root.join("prefixes/games/prefix.toml");
        fs::write(
            &prefix_file,
            "schema_version = 1\n\nslug = \"games\"\n\n[defaults.env]\nWINEDEBUG = \"-all\"\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let prefix = store.load_prefix("games")?;
        assert_eq!(
            prefix.defaults.env.get("WINEDEBUG").map(String::as_str),
            Some("-all")
        );
        assert_eq!(store.list_prefixes()?.len(), 1);
        Ok(())
    }

    #[test]
    fn invalid_hand_edit_is_skipped_and_doctor_flagged() -> Result<(), StorageError> {
        let (store, root) = store("invalid-flag");
        store.create_prefix("games")?;
        fs::write(
            root.join("prefixes/games/prefix.toml"),
            "this is {{{ not toml",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        assert!(
            store.list_prefixes()?.is_empty(),
            "invalid entry must be skipped"
        );
        assert!(matches!(
            store.load_prefix("games"),
            Err(StorageError::Invalid(_))
        ));
        let health = store.tree_health()?;
        assert_eq!(
            health.invalid_files,
            [PathBuf::from("prefixes/games/prefix.toml")]
        );
        assert!(!health.is_healthy());
        Ok(())
    }

    #[test]
    fn schema_version_mismatch_is_flagged_not_read() -> Result<(), StorageError> {
        let (store, root) = store("version-mismatch");
        store.create_prefix("games")?;
        fs::write(
            root.join("prefixes/games/prefix.toml"),
            "schema_version = 2\n\nslug = \"games\"\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        assert!(store.list_prefixes()?.is_empty());
        let health = store.tree_health()?;
        assert_eq!(
            health.invalid_files,
            [PathBuf::from("prefixes/games/prefix.toml")]
        );
        Ok(())
    }

    #[test]
    fn create_never_clobbers_an_invalid_hand_edit() -> Result<(), StorageError> {
        let (store, root) = store("no-clobber");
        fs::create_dir_all(root.join("prefixes/games")).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(root.join("prefixes/games/prefix.toml"), "broken {{")
            .unwrap_or_else(|e| panic!("write: {e}"));
        let created = store.create_prefix("games")?;
        assert_eq!(
            created.slug, "games-2",
            "dedupe must sidestep the broken entry"
        );
        let original = file_text(&root, "prefixes/games/prefix.toml");
        assert_eq!(
            original, "broken {{",
            "the hand edit must survive untouched"
        );
        Ok(())
    }

    #[test]
    fn settings_are_never_overwritten_by_ensure() -> Result<(), StorageError> {
        let (store, root) = store("settings-keep");
        fs::create_dir_all(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            root.join("settings.toml"),
            "schema_version = 1\n\nresolution_order = [\"Wine\"]\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        store.create_prefix("games")?;
        assert!(
            file_text(&root, "settings.toml").contains("resolution_order = [\"Wine\"]"),
            "ensure() must not overwrite hand-edited settings"
        );
        Ok(())
    }

    #[test]
    fn missing_settings_and_orphan_dirs_are_flagged() -> Result<(), StorageError> {
        let (store, root) = store("orphan");
        store.create_prefix("games")?;
        fs::remove_file(root.join("settings.toml")).unwrap_or_else(|e| panic!("remove: {e}"));
        fs::create_dir_all(root.join("prefixes/debris")).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(root.join("prefixes/debris.txt"), "not a prefix")
            .unwrap_or_else(|e| panic!("write: {e}"));
        let health = store.tree_health()?;
        assert_eq!(health.missing_files, [PathBuf::from("settings.toml")]);
        assert_eq!(
            health.orphan_prefix_dirs,
            [PathBuf::from("prefixes/debris")]
        );
        assert!(!health.is_healthy());
        Ok(())
    }

    #[test]
    fn tree_health_reports_a_missing_root_without_init() -> Result<(), StorageError> {
        let root = temp_root("missing-root");
        let store = TreeStore::new(root);
        let health = store.tree_health()?;
        assert!(!health.tree_exists);
        assert_eq!(health.missing_dirs.len(), TREE_DIRS.len());
        assert!(!health.is_healthy());
        Ok(())
    }

    #[test]
    fn app_files_round_trip_and_delete() -> Result<(), StorageError> {
        let (store, _) = store("app-crud");
        let app = AppEntry {
            slug: "warpinator".to_owned(),
            exe: PathBuf::from("/prefix/drive_c/warpinator.exe"),
            kind: AppKind::Tool,
            prefix: "default".to_owned(),
            overrides: Overrides::default(),
            runner: None,
            source_installer: None,
            installed_at: None,
        };
        store.save_app(&app)?;
        assert_eq!(store.load_app("warpinator")?.slug, "warpinator");
        assert_eq!(store.list_apps()?.len(), 1);
        store.delete_app("warpinator")?;
        assert!(matches!(
            store.load_app("warpinator"),
            Err(StorageError::NotFound(_))
        ));
        Ok(())
    }

    #[test]
    fn storage_rejects_path_escaping_slugs() {
        let (store, _) = store("slug-guard");
        for evil in ["../escape", "a/b", ".hidden", "", "-lead"] {
            assert!(matches!(
                store.load_prefix(evil),
                Err(StorageError::Invalid(_))
            ));
            assert!(matches!(
                store.delete_prefix(evil),
                Err(StorageError::Invalid(_))
            ));
        }
    }

    #[test]
    fn concurrent_writers_never_tear_state() -> Result<(), StorageError> {
        let (store, _) = store("concurrent");
        store.create_prefix("shared")?;
        let writers = 8;
        let iterations = 10;
        let mut handles = Vec::new();
        for writer in 0..writers {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..iterations {
                    let mut prefix = Prefix {
                        slug: "shared".to_owned(),
                        defaults: PrefixDefaults::default(),
                    };
                    prefix.defaults.env = (0..20)
                        .map(|i| (format!("K{i}"), format!("w{writer}-{i}")))
                        .collect();
                    store
                        .save_prefix(&prefix)
                        .unwrap_or_else(|e| panic!("save: {e}"));
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap_or_else(|e| panic!("join: {e:?}"));
        }
        let loaded = store.load_prefix("shared")?;
        assert_eq!(loaded.defaults.env.len(), 20, "torn write observed");
        let writers_seen: std::collections::BTreeSet<&str> = loaded
            .defaults
            .env
            .values()
            .map(|v| v.split('-').next().unwrap_or_default())
            .collect();
        assert_eq!(writers_seen.len(), 1, "env mixed between writers");
        Ok(())
    }

    #[test]
    fn concurrent_creates_dedupe_atomically() -> Result<(), StorageError> {
        let (store, _) = store("concurrent-create");
        let writers = 8;
        let mut handles = Vec::new();
        for _ in 0..writers {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                store
                    .create_prefix("games")
                    .unwrap_or_else(|e| panic!("create: {e}"))
            }));
        }
        let mut slugs: Vec<String> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or_else(|e| panic!("join: {e:?}")).slug)
            .collect();
        slugs.sort();
        let expected: Vec<String> = (0..writers)
            .map(|i| {
                if i == 0 {
                    "games".to_owned()
                } else {
                    format!("games-{}", i + 1)
                }
            })
            .collect();
        assert_eq!(
            slugs, expected,
            "concurrent creates must dedupe, never collide"
        );
        assert_eq!(store.list_prefixes()?.len(), writers);
        Ok(())
    }

    #[test]
    fn hand_edited_slug_drift_is_invalid_everywhere() -> Result<(), StorageError> {
        let (store, root) = store("slug-drift");
        store.create_prefix("games")?;
        fs::write(
            root.join("prefixes/games/prefix.toml"),
            "schema_version = 1\n\nslug = \"renamed\"\n",
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        assert!(matches!(
            store.load_prefix("games"),
            Err(StorageError::Invalid(_))
        ));
        assert!(
            store.list_prefixes()?.is_empty(),
            "drifted entry must be skipped"
        );
        let health = store.tree_health()?;
        assert_eq!(
            health.invalid_files,
            [PathBuf::from("prefixes/games/prefix.toml")]
        );
        Ok(())
    }

    #[test]
    fn discover_and_installer_stubs_are_loud() -> Result<(), StorageError> {
        let (store, _) = store("stubs");
        let prefix = store.create_prefix("games")?;
        assert!(matches!(
            store.discover_executables(&prefix),
            Err(StorageError::Unimplemented(_))
        ));
        Ok(())
    }
}
