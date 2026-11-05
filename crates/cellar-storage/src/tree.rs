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
use cellar_core::manifest::{ManagedRecord, RunnerManifest};
use cellar_core::ports::{InstallProgress, Storage};
use cellar_core::slug;

use crate::lnk;

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
        // The validated resolution chokepoint (#61): tilde expansion plus
        // the absolute-path rule live in cellar-core, shared with the
        // provider crates — a misconfiguration dies here, before any
        // second tree can appear under $PWD.
        let base = cellar_core::xdg::data_home()
            .map_err(|error| StorageError::Config(error.to_string()))?;
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

    /// The `drive_c` root of one prefix — where an archive extracts and
    /// where a `--add` path is rooted (ADR 0001's locked prefix layout).
    /// Exposed as its own accessor because the extraction root is the tree's
    /// rule, not presentation's: the app service extracts here, discovery
    /// scans here, and a user told to `--add` from here needs the same path
    /// the other two use.
    pub fn prefix_drive_c(&self, slug: &str) -> PathBuf {
        self.prefix_dir(slug).join("drive_c")
    }

    /// The per-launch log directory (blueprint §7): a subdir of the
    /// disposable `cache/`, not a fifth top-level node — the doctor's
    /// directory sweep stays the four locked dirs (ADR 0001).
    pub fn launch_logs_dir(&self) -> PathBuf {
        self.root.join("cache/launch-logs")
    }

    /// The Start Menu / Desktop areas of one prefix (glossary: Discovery):
    /// each user's Desktop and Start Menu (the Programs folder is under it
    /// — one recursive sweep covers both), plus the all-users Start Menu —
    /// the wine directories installers write shortcuts into. Missing areas
    /// are skipped: a fresh prefix has none, and an unreadable area must
    /// not fail the session scanning it (the doctor flags deeper tree
    /// damage; discovery stays best-effort).
    fn menu_areas(&self, slug: &str) -> Vec<PathBuf> {
        const START_MENU: &str = "AppData/Roaming/Microsoft/Windows/Start Menu";
        let drive_c = self.prefix_drive_c(slug);
        let mut areas = Vec::new();
        // Every user profile that exists — wine picks the user name; Cellar
        // must not guess which one an installer wrote to.
        let users = drive_c.join("users");
        if let Ok(user_dirs) = read_dir_sorted(&users) {
            for user in user_dirs {
                areas.push(user.join("Desktop"));
                areas.push(user.join(START_MENU));
            }
        }
        // The all-users Start Menu (ProgramData, wine's `Public` profile).
        areas.push(drive_c.join("ProgramData/Microsoft/Windows/Start Menu"));
        areas
    }

    /// Collect `*.exe` regular files under `dir`, recursively, in
    /// deterministic (sorted) order — the flat counterpart of the `.lnk`
    /// decode (#31). Symlinks are skipped, not followed: an installer may
    /// have planted anything in a prefix, and the scan must stay inside the
    /// prefix's areas — no escapes, no link cycles. Candidates whose exe
    /// was already offered (via a `.lnk` target) are skipped — the
    /// shortcut's label wins.
    fn collect_exes(
        dir: &Path,
        out: &mut Vec<Candidate>,
        seen: &mut BTreeSet<PathBuf>,
    ) -> Result<(), StorageError> {
        let Ok(entries) = read_dir_sorted(dir) else {
            // Missing or unreadable area: nothing to collect there (the
            // area sweep already degrades gracefully — see `menu_areas`).
            return Ok(());
        };
        for path in entries {
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                Self::collect_exes(&path, out, seen)?;
            } else if meta.is_file() && has_extension(&path, "exe") && seen.insert(path.clone()) {
                let label = path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default();
                out.push(Candidate { exe: path, label });
            }
        }
        Ok(())
    }

    /// Collect the `.lnk` shortcuts under `dir` (recursive, sorted, same
    /// symlink rules as the flat scan), decoding each target and resolving
    /// it against the prefix's `drive_c`. A shortcut whose target is not a
    /// real exe inside the prefix is skipped — discovery is best-effort
    /// (blueprint §8: never guesses, never auto-registers; a `.lnk` that
    /// cannot be read or decoded is noise, not a session failure).
    fn collect_shortcut_candidates(
        dir: &Path,
        drive_c: &Path,
        out: &mut Vec<Candidate>,
        seen: &mut BTreeSet<PathBuf>,
    ) -> Result<(), StorageError> {
        let Ok(entries) = read_dir_sorted(dir) else {
            return Ok(());
        };
        for path in entries {
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                Self::collect_shortcut_candidates(&path, drive_c, out, seen)?;
            } else if meta.is_file() && has_extension(&path, "lnk") {
                let Ok(bytes) = fs::read(&path) else {
                    continue;
                };
                let Some(target) = lnk::parse_lnk_target(&bytes) else {
                    continue;
                };
                let Some(exe) = resolve_lnk_target(drive_c, &target) else {
                    continue;
                };
                if seen.insert(exe.clone()) {
                    let label = path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    out.push(Candidate { exe, label });
                }
            }
        }
        Ok(())
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
        // The per-launch log directory (blueprint §7) — a subdir of `cache`,
        // so `TREE_DIRS` and the health sweep keep the four locked top-level
        // nodes.
        fs::create_dir_all(self.launch_logs_dir())
            .map_err(|e| io_err(&self.launch_logs_dir(), &e))?;
        if !self.settings_path().exists() {
            Self::write_envelope(&self.settings_path(), &Settings::default())?;
        }
        Ok(())
    }

    /// Read a file as an entity: TOML parse (unknown fields ignored — future
    /// fields must not break older binaries), then schema check. A malformed
    /// or version-mismatched file is `Invalid` — hand-edit damage degrades to
    /// a skipped, doctor-flagged entry, never a rewrite (ADR 0001).
    pub(crate) fn read_envelope<T: DeserializeOwned>(path: &Path) -> Result<T, StorageError> {
        let text = fs::read_to_string(path).map_err(|e| io_err(path, &e))?;
        // `Invalid`'s payload is a whole sentence (#45), so the parse
        // failures name the file and what is wrong with it rather than
        // handing the renderer a bare path.
        let meta: FileMeta = toml::from_str(&text)
            .map_err(|e| StorageError::Invalid(unparseable(path, Half::Header, &e)))?;
        if meta.schema_version != SCHEMA_VERSION {
            return Err(StorageError::Invalid(format!(
                "{}: schema version {} (the tree reads {SCHEMA_VERSION})",
                path.display(),
                meta.schema_version
            )));
        }
        toml::from_str(&text)
            .map_err(|e| StorageError::Invalid(unparseable(path, Half::Entity, &e)))
    }

    /// Write an entity atomically: `schema_version` header + TOML body via a
    /// unique temp file in the same directory, then `rename` over the target.
    /// A reader or concurrent writer never observes a partial file.
    pub(crate) fn write_envelope<T: Serialize + ?Sized>(
        path: &Path,
        data: &T,
    ) -> Result<(), StorageError> {
        let dir = path.parent().ok_or_else(|| {
            StorageError::Io(format!("{} has no parent directory", path.display()))
        })?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let tmp = dir.join(format!(
            ".{name}.tmp.{}.{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let contents = Self::envelope_contents(path, data)?;
        let result = (|| -> std::io::Result<()> {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
            fs::rename(&tmp, path)?;
            // The rename's directory entry must reach the disk too, or a
            // power cut reverts a recorded fact to absent (#62).
            cellar_core::durability::sync_parent_dir(path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.map_err(|e| io_err(path, &e))
    }

    /// The on-disk text of one entity: the `schema_version` header plus the
    /// TOML body. Shared by the temp+rename upsert and the exclusive
    /// claim, so a claimed entry is byte-identical to a saved one.
    fn envelope_contents<T: Serialize + ?Sized>(
        path: &Path,
        data: &T,
    ) -> Result<String, StorageError> {
        let body = toml::to_string(data).map_err(|e| {
            StorageError::Invalid(format!("cannot serialize {}: {e}", path.display()))
        })?;
        Ok(format!("schema_version = {SCHEMA_VERSION}\n\n{body}"))
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

    /// Flag an entry whose registered exe is missing from disk — the
    /// tree-wide sweep of the launch check phase (blueprint §7 disposition:
    /// "exe missing → re-register"). Only a regular file at the exe path
    /// counts as present (a directory squatting the path flags too); real
    /// I/O errors propagate loudly.
    fn flag_missing_exe(app: &AppEntry, health: &mut TreeHealth) -> Result<(), StorageError> {
        match fs::metadata(&app.exe) {
            Ok(meta) if meta.is_file() => Ok(()),
            Ok(_) => {
                health.missing_exes.push(app.slug.clone());
                Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                health.missing_exes.push(app.slug.clone());
                Ok(())
            }
            Err(err) => Err(io_err(&app.exe, &err)),
        }
    }
}

impl cellar_core::ports::__sealed::Sealed for TreeStore {}

impl Storage for TreeStore {
    fn data_root(&self) -> &Path {
        &self.root
    }

    fn prefix_dir(&self, slug: &str) -> PathBuf {
        // Explicit delegation to the inherent `TreeStore::prefix_dir` — the
        // layout stays the adapter's; inherent methods would shadow this
        // trait method, so the UFCS call is deliberate, not recursion.
        TreeStore::prefix_dir(self, slug)
    }

    fn launch_logs_dir(&self) -> PathBuf {
        // Same deliberate UFCS delegation as `prefix_dir`: the adapter owns
        // the layout, the port exposes it (#29).
        TreeStore::launch_logs_dir(self)
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
        // never more. The removal is synced so deleted state cannot
        // resurrect after a crash (#62).
        fs::remove_dir_all(&dir).map_err(|e| io_err(&dir, &e))?;
        cellar_core::durability::sync_parent_dir(&dir).map_err(|e| io_err(&dir, &e))
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

    fn claim_app(&self, app: &AppEntry) -> Result<(), StorageError> {
        Self::require_valid_slug("app", &app.slug)?;
        self.ensure_tree()?;
        let file = self.app_file(&app.slug);
        let contents = Self::envelope_contents(&file, app)?;
        // The exclusive create *is* the claim (#60): `create_new` fails
        // when the slug was taken since the caller's dedupe snapshot, so
        // two processes registering the same display name can no longer
        // collapse onto one file with no error anywhere. This is the
        // primitive `create_prefix` already uses for its directory.
        let mut handle = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file)
        {
            Ok(handle) => handle,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(StorageError::Exists(file.display().to_string()));
            }
            Err(err) => return Err(io_err(&file, &err)),
        };
        // The claim is held and the file is ours alone, so a failure past
        // this point is undone rather than left squatting the slug. Only a
        // hard crash between here and `sync` leaves anything behind: an
        // empty entry file, which degrades to the skipped-and-doctor-
        // flagged state the tree already models for hand-edit damage
        // (ADR 0001), never to a lost entry.
        if let Err(err) = handle
            .write_all(contents.as_bytes())
            .and_then(|()| handle.sync_all())
        {
            drop(handle);
            let _ = fs::remove_file(&file);
            return Err(io_err(&file, &err));
        }
        drop(handle);
        // The new directory entry must reach the disk, or a power cut
        // reverts the claim to absent and the next registration re-uses a
        // name that was already handed out (#62).
        cellar_core::durability::sync_parent_dir(&file).map_err(|e| io_err(&file, &e))
    }

    fn delete_app(&self, slug: &str) -> Result<(), StorageError> {
        Self::require_valid_slug("app", slug)?;
        self.ensure_tree()?;
        let file = self.app_file(slug);
        if !file.exists() {
            return Err(StorageError::NotFound(file.display().to_string()));
        }
        fs::remove_file(&file).map_err(|e| io_err(&file, &e))?;
        // Deleted state cannot resurrect after a crash (#62).
        cellar_core::durability::sync_parent_dir(&file).map_err(|e| io_err(&file, &e))
    }

    fn list_app_slugs(&self) -> Result<Vec<String>, StorageError> {
        self.ensure_tree()?;
        let mut slugs = Vec::new();
        for path in read_dir_sorted(&self.root.join("apps"))? {
            if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            // Every stem counts — a broken hand-edited entry is part of the
            // dedupe domain, so a fresh install never overwrites it
            // (ADR 0001), mirroring how prefix creates sidestep broken
            // directories.
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                slugs.push(stem.to_owned());
            }
        }
        Ok(slugs)
    }

    fn canonicalize_exe(&self, path: &Path) -> Result<PathBuf, StorageError> {
        let canonical = fs::canonicalize(path).map_err(|e| io_err(path, &e))?;
        if !canonical.is_file() {
            return Err(StorageError::Invalid(format!(
                "{} is not a file",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    fn tree_health(&self) -> Result<TreeHealth, StorageError> {
        let mut health = TreeHealth {
            root: self.root.clone(),
            tree_exists: true,
            missing_dirs: Vec::new(),
            missing_files: Vec::new(),
            invalid_files: Vec::new(),
            orphan_prefix_dirs: Vec::new(),
            missing_exes: Vec::new(),
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
                match Self::read_app_at(&path) {
                    Ok(app) => Self::flag_missing_exe(&app, &mut health)?,
                    Err(StorageError::NotFound(_)) => {}
                    Err(StorageError::Invalid(_)) => health.invalid_files.push(relative),
                    Err(other) => return Err(other),
                }
            }
        }
        Ok(health)
    }

    fn discover_executables(&self, prefix: &Prefix) -> Result<Vec<Candidate>, StorageError> {
        // Discovery (this slice, #31): every `*.exe` regular file found
        // recursively under the prefix's menu and desktop areas (the flat
        // scan, #30), joined with the `.lnk` targets that resolve to real
        // exes inside the prefix — the shortcut's label wins over the flat
        // scan's stem, and the same target from two shortcuts appears
        // once. Deterministic: areas in fixed order (sorted users,
        // per-user Desktop before Start Menu, then the all-users menu),
        // entries sorted within each. Never auto-registers (blueprint §8).
        let drive_c = self.prefix_drive_c(&prefix.slug);
        let mut candidates = Vec::new();
        let mut seen = BTreeSet::new();
        for area in self.menu_areas(&prefix.slug) {
            Self::collect_shortcut_candidates(&area, &drive_c, &mut candidates, &mut seen)?;
            Self::collect_exes(&area, &mut candidates, &mut seen)?;
        }
        Ok(candidates)
    }

    fn install_managed(
        &self,
        manifest: &RunnerManifest,
        version: &str,
        progress: &mut dyn FnMut(InstallProgress),
    ) -> Result<PathBuf, StorageError> {
        crate::installer::install(&self.root, manifest, version, progress)
    }

    fn managed_inventory(&self) -> Result<Vec<ManagedRecord>, StorageError> {
        crate::installer::inventory(&self.root)
    }
}

/// Which half of a tree file a parse failure came from (#45).
///
/// The two parses are not two regions of the document — `FileMeta` parses
/// the *whole* file and ignores unknown fields, so a TOML syntax error
/// anywhere (body included) fails the header parse first, and a failure
/// here means "the file does not even read as a tree file". [`Entity`] is
/// reached only when the file parsed cleanly but is missing a required
/// field or has one of the wrong type — the hand-edit damage a user is
/// likelier to cause, and the one where naming the half helps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Half {
    Header,
    Entity,
}

/// A whole sentence for a tree file that does not parse (#45):
/// `<path>: <what> (<detail>)`. The detail is the `toml` error verbatim —
/// it carries the line and column, which is what a hand-editor needs.
fn unparseable(path: &Path, half: Half, error: &toml::de::Error) -> String {
    let what = match half {
        Half::Header => {
            "not readable as a tree file (a TOML syntax error, or a `schema_version` that is not a number)"
        }
        Half::Entity => "reads as TOML but is missing a required field, or one has the wrong type",
    };
    format!("{}: {what} ({error})", path.display())
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

/// Whether `path`'s extension matches `extension`, case-insensitively —
/// Windows file naming, applied to both the flat-scan exe check and the
/// `.lnk` shortcut check.
fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
}

/// Resolve a `.lnk` target — a Windows path like
/// `C:\Program Files\My Game\game.exe` — against the prefix's `drive_c`
/// (wine maps `C:\` there). The walk is case-insensitive per component
/// (Windows filesystems are; wine's are not), never follows symlinks (the
/// flat scan's containment rule, mirrored here), and only yields a real
/// `*.exe` file. Anything else — non-`C:` drives, UNC or relative paths,
/// missing nodes, folder or non-exe targets — is `None`: not a candidate.
fn resolve_lnk_target(drive_c: &Path, target: &str) -> Option<PathBuf> {
    let bytes = target.trim().as_bytes();
    if bytes.len() < 3 || bytes[1] != b':' || !bytes[0].eq_ignore_ascii_case(&b'c') {
        return None;
    }
    let mut current = drive_c.to_path_buf();
    for component in target
        .get(2..)?
        .split(['\\', '/'])
        .filter(|c| !c.is_empty())
    {
        current = find_child_ci(&current, component)?;
    }
    let Ok(meta) = fs::metadata(&current) else {
        return None;
    };
    if !meta.is_file() || !has_extension(&current, "exe") {
        return None;
    }
    Some(current)
}

/// The child of `dir` whose name matches `name` case-insensitively — or
/// `None`. A symlink matching the name is refused, not followed: the walk
/// stays inside the prefix (installers can plant anything).
fn find_child_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if file_name.eq_ignore_ascii_case(name) {
            let meta = fs::symlink_metadata(&path).ok()?;
            if meta.file_type().is_symlink() {
                return None;
            }
            return Some(path);
        }
    }
    None
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

    /// A minimal entry for the claim tests; only the slug and exe matter
    /// to the storage layer.
    fn entry_for(slug: &str) -> AppEntry {
        AppEntry {
            slug: slug.to_owned(),
            exe: PathBuf::from(format!("/games/{slug}.exe")),
            kind: AppKind::Game,
            prefix: "default".to_owned(),
            overrides: Overrides::default(),
            runner: None,
            source_installer: None,
            installed_at: None,
        }
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
    fn discovery_flat_scan_finds_exes_across_menu_and_desktop_areas() -> Result<(), StorageError> {
        // An installer dropped a game next to its shortcut — nested Start
        // Menu dirs, several user profiles, an all-users shortcut, and
        // non-exe noise the scan must ignore. The opaque `.lnk` is skipped:
        // it carries no valid shell-link header, so it yields no target
        // (the decode itself is covered below).
        let (store, root) = store("discovery");
        let prefix = store.create_prefix("default")?;
        let prefix_dir = root.join("prefixes/default");
        let me_desktop = prefix_dir.join("drive_c/users/me/Desktop");
        let me_menu = prefix_dir
            .join("drive_c/users/me/AppData/Roaming/Microsoft/Windows/Start Menu/Programs/My Game");
        let other_desktop = prefix_dir.join("drive_c/users/another/Desktop");
        let all_users =
            prefix_dir.join("drive_c/ProgramData/Microsoft/Windows/Start Menu/Programs");
        for dir in [&me_desktop, &me_menu, &other_desktop, &all_users] {
            fs::create_dir_all(dir).unwrap_or_else(|e| panic!("mkdir {dir:?}: {e}"));
        }
        fs::write(me_desktop.join("Tool.exe"), "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(me_desktop.join("readme.txt"), "hi").unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(me_menu.join("Nested Game.EXE"), "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(me_menu.join("My Game.lnk"), "opaque").unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(other_desktop.join("Other.exe"), "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(all_users.join("Shared.exe"), "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        let candidates = store.discover_executables(&prefix)?;
        let found: Vec<&str> = candidates.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            found,
            ["Other", "Tool", "Nested Game", "Shared"],
            "every user's areas (sorted) and the all-users menu, exes only"
        );
        assert!(
            candidates
                .iter()
                .all(|c| c.exe.starts_with(&prefix_dir) && c.exe.extension().is_some()),
            "candidates are real files under the prefix"
        );
        Ok(())
    }

    #[test]
    fn discovery_resolves_lnk_targets_into_candidates() -> Result<(), StorageError> {
        // A shortcut on the Desktop pointing at an app exe deep in
        // program files: the decoded target becomes a candidate labelled
        // with the shortcut's display name, exactly once (the flat scan
        // would also find the exe — the shortcut's label wins).
        let (store, root) = store("discovery-lnk");
        let prefix = store.create_prefix("default")?;
        let prefix_dir = root.join("prefixes/default").join("drive_c");
        let game_dir = prefix_dir.join("Program Files/My Game");
        fs::create_dir_all(&game_dir).unwrap_or_else(|e| panic!("mkdir: {e}"));
        let game_exe = game_dir.join("game.exe");
        fs::write(&game_exe, "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        let desktop = prefix_dir.join("users/me/Desktop");
        fs::create_dir_all(&desktop).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            desktop.join("Play My Game.lnk"),
            crate::lnk::build_lnk(true, true, r"C:\Program Files\My Game\game.exe", ""),
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(desktop.join("notes.txt"), "hi").unwrap_or_else(|e| panic!("write: {e}"));
        let candidates = store.discover_executables(&prefix)?;
        assert_eq!(
            candidates,
            [Candidate {
                exe: game_exe,
                label: "Play My Game".to_owned(),
            }],
            "the shortcut name labels the resolved exe, exactly once"
        );
        Ok(())
    }

    #[test]
    fn discovery_resolves_targets_case_insensitively() -> Result<(), StorageError> {
        // Installers write shortcuts with the exact case they remember;
        // wine's drive_c may differ. Windows naming is case-insensitive —
        // the resolution walks every component that way.
        let (store, root) = store("discovery-lnk-case");
        let prefix = store.create_prefix("default")?;
        let prefix_dir = root.join("prefixes/default").join("drive_c");
        fs::create_dir_all(prefix_dir.join("program files/game"))
            .unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(prefix_dir.join("program files/game/Game.exe"), "MZ")
            .unwrap_or_else(|e| panic!("write: {e}"));
        let menu = prefix_dir
            .join("users/me/AppData/Roaming/Microsoft/Windows/Start Menu/Programs/My Game");
        fs::create_dir_all(&menu).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            menu.join("Game.lnk"),
            crate::lnk::build_lnk(false, false, r"C:\PROGRAM FILES\GAME\GAME.EXE", ""),
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let candidates = store.discover_executables(&prefix)?;
        assert_eq!(
            candidates
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            ["Game"],
            "the ANSI shortcut's target resolves through differing case"
        );
        assert!(
            candidates[0].exe.ends_with("program files/game/Game.exe"),
            "the resolved path is the on-disk one: {}",
            candidates[0].exe.display()
        );
        Ok(())
    }

    #[test]
    fn discovery_resolves_a_non_ascii_ansi_target() -> Result<(), StorageError> {
        // A Western installer writes `Café\game.exe` with the `é` as a
        // single CP1252 byte; the on-disk name is UTF-8. The decode maps
        // the byte back to `é`, so the walk matches the component exactly.
        let (store, root) = store("discovery-lnk-ansi");
        let prefix = store.create_prefix("default")?;
        let prefix_dir = root.join("prefixes/default").join("drive_c");
        let cafe = prefix_dir.join("Program Files/Café");
        fs::create_dir_all(&cafe).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(cafe.join("game.exe"), "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        let desktop = prefix_dir.join("users/me/Desktop");
        fs::create_dir_all(&desktop).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(
            desktop.join("Caf.lnk"),
            crate::lnk::build_lnk_raw(false, false, b"C:\\Program Files\\Caf\xE9\\game.exe", b""),
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let candidates = store.discover_executables(&prefix)?;
        assert_eq!(
            candidates,
            [Candidate {
                exe: cafe.join("game.exe"),
                label: "Caf".to_owned(),
            }],
            "the CP1252 target byte resolves to the UTF-8 on-disk name"
        );
        Ok(())
    }

    #[test]
    fn discovery_skips_shortcuts_without_a_resolvable_exe_target() -> Result<(), StorageError> {
        // Dangling targets (uninstalled apps), folder targets, other-drive
        // and UNC targets, and unparseable blobs are all skipped — a
        // shortcut is never a candidate unless it names a real exe inside
        // the prefix.
        let (store, root) = store("discovery-lnk-skip");
        let prefix = store.create_prefix("default")?;
        let prefix_dir = root.join("prefixes/default").join("drive_c");
        let desktop = prefix_dir.join("users/me/Desktop");
        fs::create_dir_all(&desktop).unwrap_or_else(|e| panic!("mkdir: {e}"));
        let write_link = |name: &str, blob: &[u8]| {
            fs::write(desktop.join(name), blob).unwrap_or_else(|e| panic!("write {name}: {e}"));
        };
        // Target missing on disk.
        write_link(
            "Gone.lnk",
            &crate::lnk::build_lnk(true, false, r"C:\Games\Gone\gone.exe", ""),
        );
        // Target is a folder, not a file.
        fs::create_dir_all(prefix_dir.join("Games/Folder"))
            .unwrap_or_else(|e| panic!("mkdir: {e}"));
        write_link(
            "Folder.lnk",
            &crate::lnk::build_lnk(true, false, r"C:\Games\Folder", ""),
        );
        // Target on another drive.
        write_link(
            "Other Drive.lnk",
            &crate::lnk::build_lnk(true, false, r"D:\Games\game.exe", ""),
        );
        // Non-exe target.
        fs::write(prefix_dir.join("Games/readme.txt"), "hi")
            .unwrap_or_else(|e| panic!("write: {e}"));
        write_link(
            "Not An Exe.lnk",
            &crate::lnk::build_lnk(false, false, r"C:\Games\readme.txt", ""),
        );
        // Not a shell link at all.
        write_link("Garbage.lnk", b"definitely not a shortcut");
        assert_eq!(
            store.discover_executables(&prefix)?,
            Vec::new(),
            "no candidate from any of these shortcuts"
        );
        Ok(())
    }

    #[test]
    fn discovery_merges_flat_and_shortcut_candidates_and_dedupes() -> Result<(), StorageError> {
        // The exe a shortcut names and the flat scan finds is offered once
        // (shortcut label wins); a shortcut-only exe and a flat-only exe
        // both appear; two shortcuts to the same exe offer it once.
        let (store, root) = store("discovery-lnk-merge");
        let prefix = store.create_prefix("default")?;
        let prefix_dir = root.join("prefixes/default").join("drive_c");
        let game_dir = prefix_dir.join("Program Files/My Game");
        fs::create_dir_all(&game_dir).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(game_dir.join("game.exe"), "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(game_dir.join("launcher.exe"), "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        let desktop = prefix_dir.join("users/me/Desktop");
        fs::create_dir_all(&desktop).unwrap_or_else(|e| panic!("mkdir: {e}"));
        // Two shortcuts for the same game exe, plus one for the launcher.
        let target = r"C:\Program Files\My Game\game.exe";
        fs::write(
            desktop.join("Play My Game.lnk"),
            crate::lnk::build_lnk(true, true, target, ""),
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            desktop.join("My Game.lnk"),
            crate::lnk::build_lnk(false, false, target, ""),
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(
            desktop.join("Launcher.lnk"),
            crate::lnk::build_lnk(true, false, r"C:\Program Files\My Game\launcher.exe", ""),
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        let candidates = store.discover_executables(&prefix)?;
        let found: Vec<(&str, &str)> = candidates
            .iter()
            .map(|c| {
                (
                    c.label.as_str(),
                    c.exe
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            found,
            [("Launcher", "launcher.exe"), ("My Game", "game.exe"),],
            "flat-only exes still appear; the first shortcut in sorted order names the exe"
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn discovery_shortcut_resolution_refuses_symlinked_components() -> Result<(), StorageError> {
        use std::os::unix::fs::symlink;

        // A shortcut can be planted by anyone; its target path must not
        // walk out of the prefix through a symlinked directory — the
        // containment rule of the flat scan, mirrored in resolution.
        let (store, root) = store("discovery-lnk-links");
        let prefix = store.create_prefix("default")?;
        let prefix_dir = root.join("prefixes/default").join("drive_c");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(outside.join("sneaky.exe"), "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        let desktop = prefix_dir.join("users/me/Desktop");
        fs::create_dir_all(&desktop).unwrap_or_else(|e| panic!("mkdir: {e}"));
        symlink(&outside, prefix_dir.join("Escape")).unwrap_or_else(|e| panic!("symlink: {e}"));
        fs::write(
            desktop.join("Sneaky.lnk"),
            crate::lnk::build_lnk(true, false, r"C:\Escape\sneaky.exe", ""),
        )
        .unwrap_or_else(|e| panic!("write: {e}"));
        assert_eq!(
            store.discover_executables(&prefix)?,
            Vec::new(),
            "the resolution never follows a symlinked component"
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn discovery_skips_symlinks_and_stays_inside_the_prefix() -> Result<(), StorageError> {
        use std::os::unix::fs::symlink;

        let (store, root) = store("discovery-links");
        let prefix = store.create_prefix("default")?;
        let prefix_dir = root.join("prefixes/default");
        let desktop = prefix_dir.join("drive_c/users/me/Desktop");
        fs::create_dir_all(&desktop).unwrap_or_else(|e| panic!("mkdir: {e}"));
        // An outside directory carrying an exe, linked into the menu area —
        // the scan must not follow it out of the prefix.
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(outside.join("sneaky.exe"), "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        symlink(&outside, desktop.join("escape")).unwrap_or_else(|e| panic!("symlink: {e}"));
        // A link cycle (a menu subdir linking back up) must terminate too.
        symlink(&desktop, desktop.join("cycle")).unwrap_or_else(|e| panic!("symlink: {e}"));
        fs::write(desktop.join("Real.exe"), "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        let candidates = store.discover_executables(&prefix)?;
        let found: Vec<&str> = candidates.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            found,
            ["Real"],
            "no escapes, no cycles — the scan stays inside the prefix's areas"
        );
        Ok(())
    }

    #[test]
    fn discovery_on_a_fresh_prefix_is_empty() -> Result<(), StorageError> {
        // No drive_c yet, no areas — the scan yields nothing, never fails.
        let (store, _root) = store("discovery-empty");
        let prefix = store.create_prefix("default")?;
        assert_eq!(store.discover_executables(&prefix)?, Vec::new());
        Ok(())
    }

    #[test]
    fn first_run_creates_the_per_launch_log_directory() -> Result<(), StorageError> {
        // Blueprint §7: game output always goes to `cache/launch-logs/` — a
        // subdir of the disposable cache, created with the tree (#29).
        let (store, root) = store("launch-logs");
        store.create_prefix("default")?;
        assert!(root.join("cache/launch-logs").is_dir());
        assert_eq!(
            store.launch_logs_dir(),
            root.join("cache/launch-logs"),
            "the layout stays the adapter's knowledge"
        );
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
    fn canonicalize_exe_resolves_real_files_only() -> Result<(), StorageError> {
        let (store, root) = store("canonical-exe");
        let dir = root.join("bin");
        fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir: {e}"));
        let exe = dir.join("game.exe");
        fs::write(&exe, "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        let canonical = store.canonicalize_exe(&exe)?;
        assert_eq!(
            canonical,
            fs::canonicalize(&exe).unwrap_or_else(|e| panic!("canon: {e}"))
        );
        assert!(matches!(
            store.canonicalize_exe(&dir.join("gone.exe")),
            Err(StorageError::NotFound(_))
        ));
        assert!(matches!(
            store.canonicalize_exe(&dir),
            Err(StorageError::Invalid(_))
        ));
        Ok(())
    }

    #[test]
    fn app_slug_domain_covers_broken_hand_edits() -> Result<(), StorageError> {
        let (store, root) = store("app-slugs");
        store.save_app(&AppEntry {
            slug: "alpha".to_owned(),
            exe: PathBuf::from("/x/alpha.exe"),
            kind: AppKind::Game,
            prefix: "default".to_owned(),
            overrides: Overrides::default(),
            runner: None,
            source_installer: None,
            installed_at: None,
        })?;
        fs::write(root.join("apps/broken.toml"), "not toml {{{")
            .unwrap_or_else(|e| panic!("write: {e}"));
        fs::write(root.join("apps/notes.txt"), "ignored").unwrap_or_else(|e| panic!("write: {e}"));
        assert_eq!(
            store.list_app_slugs()?,
            ["alpha", "broken"],
            "the dedupe domain must include broken entries and skip non-toml"
        );
        Ok(())
    }

    #[test]
    fn tree_health_flags_entries_whose_exe_disappeared() -> Result<(), StorageError> {
        let (store, root) = store("missing-exe");
        let exe = root.join("drive_c/game.exe");
        fs::create_dir_all(exe.parent().unwrap_or(Path::new(".")))
            .unwrap_or_else(|e| panic!("mkdir: {e}"));
        fs::write(&exe, "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        store.save_app(&AppEntry {
            slug: "game".to_owned(),
            exe: exe.clone(),
            kind: AppKind::Game,
            prefix: "default".to_owned(),
            overrides: Overrides::default(),
            runner: None,
            source_installer: None,
            installed_at: None,
        })?;
        assert!(store.tree_health()?.is_healthy());
        fs::remove_file(&exe).unwrap_or_else(|e| panic!("remove: {e}"));
        let health = store.tree_health()?;
        assert_eq!(health.missing_exes, ["game"]);
        assert!(!health.is_healthy());
        // The entry stays registered — only its status changes.
        assert_eq!(store.list_apps()?.len(), 1);
        Ok(())
    }

    #[test]
    fn a_directory_squatting_the_exe_path_is_flagged_too() -> Result<(), StorageError> {
        let (store, root) = store("exe-dir-squat");
        let exe = root.join("drive_c/game.exe");
        fs::create_dir_all(&exe).unwrap_or_else(|e| panic!("mkdir: {e}"));
        store.save_app(&AppEntry {
            slug: "game".to_owned(),
            exe: exe.clone(),
            kind: AppKind::Game,
            prefix: "default".to_owned(),
            overrides: Overrides::default(),
            runner: None,
            source_installer: None,
            installed_at: None,
        })?;
        let health = store.tree_health()?;
        assert_eq!(health.missing_exes, ["game"]);
        assert!(!health.is_healthy());
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

    /// The #60 regression at the storage layer: a lost claim is *reported*,
    /// never an overwrite, and re-deduping after the loss hands out a
    /// distinct file. Before the claim, `save_app`'s temp+rename
    /// overwrote unconditionally, so 8 concurrent registrations of one
    /// display name all succeeded onto a single file.
    ///
    /// The retry lives in the app service (`claim_slug`), so this drives
    /// the same loop the service does rather than asserting a policy the
    /// storage layer does not own.
    #[test]
    fn a_lost_claim_reports_exists_and_the_retry_takes_a_new_file() -> Result<(), StorageError> {
        let (store, _) = store("claim-retry");
        let mut first = entry_for("mygame");
        store.claim_app(&first)?;
        // The second writer's first attempt loses.
        assert!(
            matches!(store.claim_app(&first), Err(StorageError::Exists(_))),
            "the incumbent's slug must report Exists, not clobber"
        );
        // Re-dedupe against the live slug set, as the service does.
        let taken: std::collections::BTreeSet<String> =
            store.list_app_slugs()?.into_iter().collect();
        first.slug = cellar_core::slug::dedupe_slug("mygame", &taken);
        store.claim_app(&first)?;
        assert_eq!(first.slug, "mygame-2");
        assert_eq!(store.list_apps()?.len(), 2, "both entries must survive");
        Ok(())
    }

    /// The concurrency property that matters: writers racing for one slug,
    /// each re-deduping on a lost claim, end up with distinct files and no
    /// errors — the shape `concurrent_creates_dedupe_atomically` pins for
    /// prefixes, applied to entries.
    #[test]
    fn concurrent_claims_dedupe_to_distinct_files() -> Result<(), StorageError> {
        let (store, _) = store("concurrent-claim");
        let writers = 8;
        let mut handles = Vec::new();
        for _ in 0..writers {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                let mut entry = entry_for("mygame");
                loop {
                    match store.claim_app(&entry) {
                        Ok(()) => return entry.slug,
                        Err(StorageError::Exists(_)) => {}
                        Err(err) => panic!("claim: {err}"),
                    }
                    let taken: std::collections::BTreeSet<String> = store
                        .list_app_slugs()
                        .unwrap_or_else(|e| panic!("slugs: {e}"))
                        .into_iter()
                        .collect();
                    entry.slug = cellar_core::slug::dedupe_slug("mygame", &taken);
                }
            }));
        }
        let mut claimed: Vec<String> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or_else(|e| panic!("join: {e:?}")))
            .collect();
        claimed.sort();
        let mut expected: Vec<String> = (0..writers)
            .map(|i| {
                if i == 0 {
                    "mygame".to_owned()
                } else {
                    format!("mygame-{}", i + 1)
                }
            })
            .collect();
        expected.sort();
        assert_eq!(
            claimed, expected,
            "concurrent claims must dedupe, never collide"
        );
        assert_eq!(
            store.list_apps()?.len(),
            writers,
            "every registration must survive — no lost update"
        );
        Ok(())
    }

    /// A claimed entry is byte-identical to a saved one: same header, same
    /// body. The claim bypasses `write_envelope`, so this pins that it did
    /// not quietly invent a second on-disk shape.
    #[test]
    fn claim_matches_the_saved_shape() -> Result<(), StorageError> {
        let (claimed_store, claimed_root) = store("claim-shape");
        claimed_store.claim_app(&entry_for("shape"))?;
        let (saved_store, saved_root) = store("claim-shape-saved");
        saved_store.save_app(&entry_for("shape"))?;
        assert_eq!(
            file_text(&claimed_root, "apps/shape.toml"),
            file_text(&saved_root, "apps/shape.toml"),
            "a claimed entry must serialize exactly like a saved one"
        );
        Ok(())
    }

    /// An invalid slug is rejected before any filesystem work, on the
    /// claim as on every other storage entry point.
    #[test]
    fn claim_validates_the_slug_first() -> Result<(), StorageError> {
        let (store, _) = store("claim-validate");
        let mut bad = entry_for("Not A Slug!");
        assert!(matches!(
            store.claim_app(&bad),
            Err(StorageError::Invalid(_))
        ));
        bad.slug = "ok".to_owned();
        store.claim_app(&bad)?;
        assert_eq!(store.list_app_slugs()?, vec!["ok".to_owned()]);
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
    fn installer_pipeline_installs_via_the_shared_module() {
        // The managed-runner pipeline landed with #34: `install_managed`
        // now delegates to the shared installer (its own tests exercise
        // fetch/verify/extract/flock/inventory; here the delegation and
        // the inventory port round-trip).
        let (store, _) = store("pipeline");
        let manifest = RunnerManifest {
            provider_id: "proton".to_owned(),
            source: cellar_core::manifest::ReleaseSource {
                url_template: "https://example.test/proton-{tag}-{arch}.tar.gz".to_owned(),
                checksum_url_template: None,
                latest_url: None,
            },
            checksum: cellar_core::manifest::ChecksumScheme::Sha512,
            archive: cellar_core::manifest::ArchiveLayout::ExtractsToSingleRootDir,
            install_kind: cellar_core::manifest::InstallKind::CompatTool,
        };
        // A missing artifact is a pipeline failure, not a stub.
        let err = store
            .install_managed(&manifest, "9.0-4", &mut |_| {})
            .expect_err("a missing artifact fails the pipeline");
        assert!(matches!(err, StorageError::Artifact(_)) || matches!(err, StorageError::Io(_)));
        assert!(store.managed_inventory().unwrap().is_empty());
    }
}
