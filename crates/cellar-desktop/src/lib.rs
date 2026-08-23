//! The Cellar desktop integration adapter (blueprint §5): `.desktop`
//! launcher entries, Rust-native icon extraction into the disposable
//! cache, and the "Open with Cellar" file association — implementing the
//! `core::ports::DesktopIntegrator` seam.
//!
//! This slice (#33) lands the crate: registering an app creates its
//! launcher entry, uninstalling removes it, a rename refreshes the entry
//! file name (identity stays the exe path — blueprint §6), icons are
//! extracted purely in Rust (superseding the legacy
//! `wrestool`/`ImageMagick` pipeline) into `<root>/cache/icons` — the
//! disposable cache, re-derivable at any time — and the whole lifecycle
//! is one-way: the adapter derives host artifacts from the `AppEntry`
//! values the app layer hands it and never reads or writes tree state.
//! No write path leads from desktop integration back into app state.
//!
//! Layout: entries live in the filesystem's `applications/` directory
//! next to the tree root — the freedesktop discovery location — named
//! `cellar-<slug>.desktop` (namespaced, so the stale sweep can own its
//! files and never touch another program's). Icons live under the tree's
//! `cache/icons/`, keyed by a hash of the canonical exe path — a rename
//! reuses the icon (the identity is the exe), and a stale cache file can
//! never be reused for a different exe.

mod entry;
mod icon;

use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cellar_core::entities::{AppEntry, AppKind};
use cellar_core::errors::DesktopError;
use cellar_core::ports::{__sealed, DesktopIntegrator};

use entry::{AssociationEntry, CATEGORY_GAME, CATEGORY_TOOL, DesktopEntry};

/// The desktop adapter the composition root injects into the generic app
/// services: constructed over the tree root and the presentation binary
/// (`std::env::current_exe()` at the composition root — the Exec target
/// of every entry; tests pass any path, the quoting is what matters).
#[derive(Debug, Clone)]
pub struct DesktopService {
    /// The tree root (`$XDG_DATA_HOME/cellar`): the entry directory is
    /// its parent's `applications/`, the icon cache its `cache/icons/`.
    data_root: PathBuf,
    /// The presentation binary every entry execs.
    executable: PathBuf,
}

impl DesktopService {
    /// The "Open with Cellar" association file name — a host-level
    /// launcher beside the entries, not an app entry: it deliberately
    /// lives OUTSIDE the `cellar-*.desktop` entry namespace, so no app
    /// slug can ever collide with it and the stale sweep never sees it.
    const ASSOCIATION_FILE: &str = "open-with-cellar.desktop";
    /// Over an explicit tree root and executable path — explicit paths
    /// keep the adapter pure layout math, mirroring `TreeStore::new`.
    pub fn new(data_root: PathBuf, executable: PathBuf) -> Self {
        Self {
            data_root,
            executable,
        }
    }

    /// The `.desktop` discovery directory: `applications/` beside the
    /// tree root (freedesktop discovery; the tree stays one movable
    /// unit — this host-facing directory is derived, never state).
    fn entries_dir(&self) -> PathBuf {
        self.data_root
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("applications")
    }

    /// The disposable icon cache: `<root>/cache/icons` (blueprint §6).
    fn icon_cache_dir(&self) -> PathBuf {
        self.data_root.join("cache").join("icons")
    }

    /// The entry file for one app: `cellar-<slug>.desktop` — the entry
    /// file name tracks the app's slug, so a rename shows up as a file
    /// rename (and the previous file is removed by the caller).
    fn entry_path(&self, slug: &str) -> PathBuf {
        self.entries_dir().join(format!("cellar-{slug}.desktop"))
    }

    /// The cache file for one exe: a hash of the canonical exe path
    /// (the identity, blueprint §6) as the file name — renames reuse the
    /// icon, and a stale file is never picked up for a different exe.
    fn icon_path(&self, exe: &Path) -> PathBuf {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        exe.hash(&mut hasher);
        self.icon_cache_dir()
            .join(format!("{:016x}.png", hasher.finish()))
    }
}

/// Remove one file we own; a missing file is fine (the stale sweep and a
/// failed remove must never block each other).
fn remove_file(path: &Path) -> Result<(), DesktopError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DesktopError::Io(format!("{}: {error}", path.display()))),
    }
}

/// Atomic write in the target directory (temp file + rename): a reader
/// never sees a torn entry, and the directory is created on demand. The
/// temp name carries a sequence so concurrent writers in one process
/// never fight over one file.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), DesktopError> {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| DesktopError::Io(format!("{}: {error}", parent.display())))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("entry");
    let tmp = parent.join(format!(
        ".{name}.tmp{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&tmp, contents)
        .map_err(|error| DesktopError::Io(format!("{}: {error}", tmp.display())))?;
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(DesktopError::Io(format!("{}: {error}", path.display())));
    }
    Ok(())
}

impl __sealed::Sealed for DesktopService {}

impl DesktopIntegrator for DesktopService {
    fn create_entry(
        &self,
        app: &AppEntry,
        previous_slug: Option<&str>,
        icon: Option<&Path>,
    ) -> Result<PathBuf, DesktopError> {
        // A rename removes the old entry file — the entry's file name
        // always tracks the app's current slug (blueprint §6 naming).
        if let Some(previous) = previous_slug {
            if previous != app.slug {
                remove_file(&self.entry_path(previous))?;
            }
        }
        let category = match app.kind {
            AppKind::Game => CATEGORY_GAME,
            AppKind::Tool => CATEGORY_TOOL,
        };
        let args = [String::from("launch"), app.slug.clone()];
        let rendered = DesktopEntry {
            name: &app.slug,
            executable: &self.executable,
            args: &args,
            icon,
            category,
        }
        .render();
        let path = self.entry_path(&app.slug);
        write_atomic(&path, rendered.as_bytes())?;
        Ok(path)
    }

    fn remove_entry(&self, app: &AppEntry) -> Result<(), DesktopError> {
        remove_file(&self.entry_path(&app.slug))?;
        remove_file(&self.icon_path(&app.exe))?;
        Ok(())
    }

    fn install_icon(&self, exe: &Path) -> Result<Option<PathBuf>, DesktopError> {
        let dest = self.icon_path(exe);
        if dest.exists() {
            return Ok(Some(dest));
        }
        let bytes = fs::read(exe)
            .map_err(|error| DesktopError::Io(format!("{}: {error}", exe.display())))?;
        // A malformed exe or missing icon resource is not an error: the
        // entry is created icon-less (the key is omitted).
        let Some(png) = icon::extract_icon_png(&bytes) else {
            return Ok(None);
        };
        write_atomic(&dest, &png)?;
        Ok(Some(dest))
    }

    fn prune_entries(&self, keep: &[&str]) -> Result<Vec<PathBuf>, DesktopError> {
        let dir = self.entries_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            // No entries directory yet: nothing to prune.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(DesktopError::Io(format!("{}: {error}", dir.display())));
            }
        };
        let mut ours = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|error| DesktopError::Io(format!("{}: {error}", dir.display())))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // `cellar-<slug>.desktop` — the namespaced entry files we own;
            // the sweep never touches another program's entries (the
            // association file lives outside this namespace by name).
            let slug = name
                .strip_prefix("cellar-")
                .and_then(|rest| rest.strip_suffix(".desktop"));
            if let Some(slug) = slug {
                ours.push((entry.path(), slug.to_owned()));
            }
        }
        ours.sort_by(|a, b| a.0.cmp(&b.0));
        let mut removed = Vec::new();
        for (path, slug) in ours {
            if !keep.contains(&slug.as_str()) {
                match fs::remove_file(&path) {
                    Ok(()) => removed.push(path),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(DesktopError::Io(format!("{}: {error}", path.display())));
                    }
                }
            }
        }
        Ok(removed)
    }

    fn set_file_association(&self) -> Result<(), DesktopError> {
        let path = self.entries_dir().join(Self::ASSOCIATION_FILE);
        let rendered = AssociationEntry {
            executable: &self.executable,
        }
        .render();
        write_atomic(&path, rendered.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cellar_core::entities::Overrides;

    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    /// A scratch data home per test — a `cellar/` tree inside its own
    /// directory, so `applications/` lands beside it exactly as in
    /// production (`$XDG_DATA_HOME/cellar` → `$XDG_DATA_HOME/applications`).
    fn root(tag: &str) -> PathBuf {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("cellar-desktop-{tag}-{}-{seq}", std::process::id()))
            .join("cellar")
    }

    fn app(slug: &str, kind: AppKind) -> AppEntry {
        AppEntry {
            slug: slug.to_owned(),
            exe: PathBuf::from(format!("/games/{slug}.exe")),
            kind,
            prefix: "default".to_owned(),
            overrides: Overrides::default(),
            runner: None,
            source_installer: None,
            installed_at: None,
        }
    }

    fn service(root: &Path) -> DesktopService {
        DesktopService::new(root.to_path_buf(), PathBuf::from("/opt/cellar/bin/cellar"))
    }

    /// The directories of the scratch tree, created on demand (writes
    /// everywhere else assume the tree exists, like the real adapter).
    fn tree(root: &Path) {
        fs::create_dir_all(root).expect("tree dir");
    }

    #[test]
    fn create_entry_writes_the_entry_and_remove_entry_undoes_it() {
        let root = root("entry-lifecycle");
        let service = service(&root);
        let path = service
            .create_entry(&app("balatro", AppKind::Game), None, None)
            .expect("entry writes");
        assert!(path.ends_with("applications/cellar-balatro.desktop"));
        let rendered = fs::read_to_string(&path).expect("entry readable");
        assert!(rendered.contains("Name=balatro\n"));
        assert!(rendered.contains("Exec=/opt/cellar/bin/cellar launch balatro\n"));
        assert!(rendered.contains("Categories=Game;\n"));
        assert!(!rendered.contains("Icon="), "icon-less entry omits the key");

        service
            .remove_entry(&app("balatro", AppKind::Game))
            .expect("removal succeeds");
        assert!(!path.exists(), "the entry is gone after uninstall");
    }

    #[test]
    fn create_entry_accepts_the_icon_reference() {
        let root = root("icon-reference");
        let service = service(&root);
        let icon = root.join("cache/icons/deadbeef.png");
        let path = service
            .create_entry(&app("balatro", AppKind::Tool), None, Some(&icon))
            .expect("entry writes");
        let rendered = fs::read_to_string(path).expect("entry readable");
        assert!(rendered.contains(&format!("Icon={}\n", icon.display())));
    }

    #[test]
    fn rename_removes_the_previous_entry_file() {
        let root = root("rename");
        let service = service(&root);
        let old = service
            .create_entry(&app("balatro", AppKind::Game), None, None)
            .expect("first entry");
        let renamed = service
            .create_entry(&app("poker-night", AppKind::Game), Some("balatro"), None)
            .expect("renamed entry");
        assert!(renamed.ends_with("applications/cellar-poker-night.desktop"));
        assert!(
            !old.exists(),
            "the old entry file is removed — the entry file name tracks the slug"
        );
        assert!(renamed.exists());
        let rendered = fs::read_to_string(&renamed).expect("entry readable");
        assert!(rendered.contains("Name=poker-night\n"));
        assert!(rendered.contains("launch poker-night\n"));
    }

    #[test]
    fn prune_entries_removes_only_our_stale_files() {
        let root = root("prune");
        let service = service(&root);
        service
            .create_entry(&app("kept", AppKind::Game), None, None)
            .expect("kept entry");
        service
            .create_entry(&app("stale", AppKind::Game), None, None)
            .expect("stale entry");
        service.set_file_association().expect("association");
        // A foreign launcher in the same directory must survive the sweep.
        let foreign = service.entries_dir().join("other-app.desktop");
        fs::write(&foreign, "[Desktop Entry]\n").expect("foreign entry");
        let removed = service.prune_entries(&["kept"]).expect("prune succeeds");
        assert_eq!(removed.len(), 1, "exactly the stale cellar entry");
        assert!(removed[0].ends_with("cellar-stale.desktop"));
        assert!(service.entries_dir().join("cellar-kept.desktop").exists());
        assert!(foreign.exists(), "another program's entry is untouched");
        assert!(
            service
                .entries_dir()
                .join("open-with-cellar.desktop")
                .exists(),
            "the association file is integration, never a stale entry"
        );
    }

    #[test]
    fn install_icon_reuses_the_cache_and_extracts_only_when_missing() {
        let root = root("icon-reuse");
        let service = service(&root);
        tree(&root);
        let exe = root.join("game.exe");
        fs::write(&exe, "MZ").expect("exe");
        // A cache file for the exe's key is reused as-is.
        let cached = service.icon_path(&exe);
        fs::create_dir_all(cached.parent().expect("cache dir")).expect("cache dir");
        fs::write(&cached, "cached-pixels").expect("cache file");
        assert_eq!(
            service.install_icon(&exe).expect("reuse"),
            Some(cached.clone()),
            "an existing cache file is reused, not re-extracted"
        );
        // Cache deleted: an icon-less exe reports None.
        fs::remove_dir_all(root.join("cache")).expect("cache removed");
        assert_eq!(service.install_icon(&exe).expect("no icon"), None);
    }

    #[test]
    fn cache_deletion_re_derives_icons() {
        let root = root("re-derive");
        let service = service(&root);
        tree(&root);
        let exe = root.join("game.exe");
        fs::write(&exe, icon::tests::fixture_exe_with_icon()).expect("exe with icon");
        let icon = service
            .install_icon(&exe)
            .expect("extraction")
            .expect("an icon");
        assert!(icon.exists());
        // Deleting the cache leaves the entry functional — the Exec line
        // never referenced the cache — and the re-derivation restores it.
        fs::remove_dir_all(root.join("cache")).expect("cache removed");
        let restored = service
            .install_icon(&exe)
            .expect("re-extraction")
            .expect("restored");
        assert!(restored.exists(), "the icon re-derives from the exe");
        let png = fs::read(&restored).expect("png readable");
        assert!(
            png.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
            "a real PNG again"
        );
    }

    #[test]
    fn association_wires_the_install_entrypoint_directly() {
        let root = root("association");
        let service = service(&root);
        service.set_file_association().expect("association writes");
        let path = service.entries_dir().join("open-with-cellar.desktop");
        let rendered = fs::read_to_string(&path).expect("association readable");
        assert!(rendered.contains("Exec=/opt/cellar/bin/cellar install %f\n"));
        assert!(rendered.contains("NoDisplay=true\n"));
        assert!(rendered.contains("MimeType=application/x-ms-dos-program;\n"));
    }

    #[test]
    fn remove_entry_is_idempotent_and_removes_the_icon() {
        let root = root("idempotent");
        let service = service(&root);
        tree(&root);
        let exe = root.join("game.exe");
        let entry = service
            .create_entry(&app("balatro", AppKind::Game), None, None)
            .expect("entry");
        fs::write(&exe, icon::tests::fixture_exe_with_icon()).expect("exe");
        let icon_path = service
            .install_icon(&exe)
            .expect("extraction")
            .expect("icon");
        // The uninstall cleanup removes the entry AND the app's icon.
        let unregistered = AppEntry {
            exe: exe.clone(),
            ..app("balatro", AppKind::Game)
        };
        service.remove_entry(&unregistered).expect("first removal");
        assert!(!entry.exists());
        assert!(!icon_path.exists(), "uninstall removes the cached icon too");
        service
            .remove_entry(&unregistered)
            .expect("second removal is fine");
    }
}
