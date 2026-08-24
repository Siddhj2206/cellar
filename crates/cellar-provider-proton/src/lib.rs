//! The Proton provider (blueprint §5): managed GE-Proton / umu-Proton and
//! discover-only Steam Proton.
//!
//! Implements `RunnerResolver` (both modes) and `ManagedRunner` (a declarative
//! manifest for the storage-owned installer pipeline, research #18: SHA-512
//! verification, the `<tag>-<arch>.tar.gz` asset shape, single-root-dir
//! extraction). Resolution follows the locked order (research #18,
//! blueprint §7): configured path → managed install (the tree's
//! `runtime/proton/` versions) → Steam's compatibility dirs (read-only
//! discovery). The scan roots are injected — the wine provider's PATH
//! seam — so resolution stays deterministic and testable without touching
//! the process environment.

use cellar_core::errors::ResolveError;
use cellar_core::manifest::{
    ArchiveLayout, ChecksumScheme, InstallKind, ReleaseSource, RunnerManifest,
};
use cellar_core::ports::{__sealed, ManagedRunner, RunnerResolver};
use cellar_core::types::{
    ConfiguredRunner, ProviderMode, ResolvedRunner, RunnerFamily, RunnerInstall, RunnerRef,
    RunnerSpec,
};

use std::env;
use std::path::{Path, PathBuf};

/// Managed GE-Proton provider.
#[derive(Debug)]
pub struct ProtonProvider {
    manifest: RunnerManifest,
    /// The tree's `runtime/` directory — the managed-install scan root
    /// (the composition root passes `TreeStore::data_root()/runtime`; the
    /// provider never guesses a data root itself).
    runtime_dir: PathBuf,
    /// Steam's compatibility-tools roots, read-only discovery.
    steam_roots: Vec<PathBuf>,
}

impl ProtonProvider {
    /// Stable provider identifier, shared by the resolver trait and the
    /// managed-runner manifest.
    pub const ID: &'static str = "proton";

    pub fn new() -> Self {
        Self::with_roots(runtime_from_env(), steam_roots_from_env())
    }

    /// Over explicit scan roots (tests, embedded use).
    pub fn with_roots(runtime_dir: PathBuf, steam_roots: Vec<PathBuf>) -> Self {
        Self {
            manifest: RunnerManifest {
                provider_id: Self::ID.to_owned(),
                source: ReleaseSource {
                    url_template: "https://github.com/GloriousEggroll/proton-ge-custom/releases/download/{tag}/{tag}-{arch}.tar.gz"
                        .to_owned(),
                    checksum_url_template: Some(
                        "https://github.com/GloriousEggroll/proton-ge-custom/releases/download/{tag}/{tag}-{arch}.sha512sum"
                            .to_owned(),
                    ),
                    latest_url: Some(
                        "https://github.com/GloriousEggroll/proton-ge-custom/releases/latest"
                            .to_owned(),
                    ),
                },
                checksum: ChecksumScheme::Sha512,
                archive: ArchiveLayout::ExtractsToSingleRootDir,
                install_kind: InstallKind::CompatTool,
            },
            runtime_dir,
            steam_roots,
        }
    }

    /// Whether a directory holds a working Proton install — the probe the
    /// managed scan and the Steam scan both use (an executable `proton`
    /// launcher at the root, research #18: "detect the launcher `proton`
    /// script").
    pub fn proton_dir_ok(dir: &Path) -> bool {
        executable_file(&dir.join("proton"))
    }

    /// Steam Proton discovery (read-only host state, research #18): the
    /// directories under the given roots that hold a working Proton
    /// install — compatibility-tools dirs (`compatibilitytools.d/*`) and
    /// Steam's own `common/Proton *` installs. Deterministic order: the
    /// roots in the order given, directories name-sorted within a root.
    pub fn scan_steam(roots: &[PathBuf]) -> Vec<SteamProton> {
        let mut found = Vec::new();
        for root in roots {
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            let mut versions: Vec<(String, PathBuf)> = entries
                .filter_map(Result::ok)
                .map(|entry| {
                    (
                        entry.file_name().to_string_lossy().into_owned(),
                        entry.path(),
                    )
                })
                .filter(|(name, _path)| {
                    if root.file_name().is_some_and(|n| n == "common") {
                        name.starts_with("Proton")
                    } else {
                        true
                    }
                })
                .filter(|(_, path)| Self::proton_dir_ok(path))
                .collect();
            versions.sort_by(|a, b| a.0.cmp(&b.0));
            found.extend(
                versions
                    .into_iter()
                    .map(|(version, dir)| SteamProton { dir, version }),
            );
        }
        found
    }
}

impl Default for ProtonProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl __sealed::Sealed for ProtonProvider {}

impl RunnerResolver for ProtonProvider {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn resolve(&self, spec: &RunnerSpec) -> Result<ResolvedRunner, ResolveError> {
        Self::resolve_inner(spec, &self.runtime_dir, &self.steam_roots)
    }
}

impl ProtonProvider {
    /// Resolution, research #18 order: configured path → managed install
    /// → Steam compat dirs. A broken configured path falls through (the
    /// locked wording is an *order*, not a hard stop — the wine
    /// provider's precedent); a version pin never falls through — a
    /// pinned-but-missing managed install is `NotInstalled` so a
    /// `SuggestInstall` names the exact reinstall.
    fn resolve_inner(
        spec: &RunnerSpec,
        runtime_dir: &Path,
        steam_roots: &[PathBuf],
    ) -> Result<ResolvedRunner, ResolveError> {
        if spec.family != RunnerFamily::Proton {
            return Err(ResolveError::Unresolvable {
                family: RunnerFamily::Proton,
            });
        }
        if let Some(configured) = &spec.configured {
            match configured {
                ConfiguredRunner::Path(path) if Self::proton_dir_ok(path) => {
                    return Ok(resolved(proton_ref(path, None)));
                }
                ConfiguredRunner::Path(_) => {}
                ConfiguredRunner::Version(version) => {
                    let install = runtime_dir.join("proton").join(version);
                    if Self::proton_dir_ok(&install) {
                        return Ok(managed_resolved(&install, version));
                    }
                    return Err(ResolveError::NotInstalled {
                        family: RunnerFamily::Proton,
                    });
                }
            }
        }
        // The managed stage: installed versions under the tree's runtime
        // dir, newest install first (a pin selected a *specific* version
        // above; without one, the newest installed wins). The same probe
        // guards every candidate — a half-installed dir is never
        // resolvable. Runtime-tree installs resolve Managed — the result
        // carries the mode tag (blueprint §5).
        if let Some((version, install)) =
            newest_installed(&runtime_dir.join("proton"), Self::proton_dir_ok)
        {
            return Ok(managed_resolved(&install, &version));
        }
        // The discover-only stage: Steam's compatibility layout, read
        // only — Cellar never owns these.
        if let Some(steam) = Self::scan_steam(steam_roots).into_iter().next() {
            return Ok(resolved(proton_ref(&steam.dir, Some(steam.version))));
        }
        Err(ResolveError::Unresolvable {
            family: RunnerFamily::Proton,
        })
    }
}

/// One discovered Steam Proton install (read-only host state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamProton {
    pub dir: PathBuf,
    pub version: String,
}

impl ManagedRunner for ProtonProvider {
    fn manifest(&self) -> &RunnerManifest {
        &self.manifest
    }
}

fn proton_ref(path: &Path, version: Option<String>) -> RunnerRef {
    RunnerRef {
        provider_id: ProtonProvider::ID.to_owned(),
        family: RunnerFamily::Proton,
        install: RunnerInstall::Discovered {
            path: path.to_path_buf(),
            version,
        },
    }
}

fn resolved(reference: RunnerRef) -> ResolvedRunner {
    ResolvedRunner {
        mode: ProviderMode::DiscoverOnly,
        reference,
    }
}

/// A runtime-tree install resolves Managed — the result carries the mode
/// tag (blueprint §5): the owner of an artifact is not a *discoverer* of
/// it.
fn managed_resolved(install: &Path, version: &str) -> ResolvedRunner {
    ResolvedRunner {
        mode: ProviderMode::Managed,
        reference: RunnerRef {
            provider_id: ProtonProvider::ID.to_owned(),
            family: RunnerFamily::Proton,
            install: RunnerInstall::Managed {
                version: version.to_owned(),
                path: install.to_path_buf(),
            },
        },
    }
}

/// The newest (by directory mtime) dir under `dir` whose `probe` passes,
/// as `(name, path)`. The shared managed-install scan.
fn newest_installed(dir: &Path, probe: fn(&Path) -> bool) -> Option<(String, PathBuf)> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok();
            (name, path, modified)
        })
        .filter(|(_, path, _)| probe(path))
        .max_by_key(|(_, _, modified)| *modified)
        .map(|(name, path, _)| (name, path))
}

/// The tree's runtime dir from the environment (`$XDG_DATA_HOME/cellar/
/// runtime`, mirroring `TreeStore::from_env`'s XDG fallback) — the
/// default construction inputs; the composition root passes the store's
/// own root explicitly.
fn runtime_from_env() -> PathBuf {
    data_home().join("cellar").join("runtime")
}

/// The Steam discovery roots from the environment (research #18: the
/// `compatibilitytools.d` layout and Steam's own `common/Proton *`
/// installs).
fn steam_roots_from_env() -> Vec<PathBuf> {
    let data = data_home();
    let mut roots = Vec::with_capacity(3);
    roots.push(data.join("Steam").join("compatibilitytools.d"));
    if let Some(home) = env::var_os("HOME") {
        roots.push(
            PathBuf::from(home)
                .join(".steam")
                .join("steam")
                .join("steamapps")
                .join("common"),
        );
    }
    roots
}

fn data_home() -> PathBuf {
    match env::var("XDG_DATA_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => env::var_os("HOME").map_or_else(
            || PathBuf::from("."),
            |home| PathBuf::from(home).join(".local").join("share"),
        ),
    }
}

/// `execvp`'s "found and executable" predicate (the wine provider's).
fn executable_file(path: &Path) -> bool {
    path.is_file() && is_executable(path)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A scratch data home rooted at a unique temp dir.
    fn home(tag: &str) -> PathBuf {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("cellar-proton-{tag}-{}-{seq}", std::process::id()))
    }

    fn write_file(path: &Path, body: &[u8], executable: bool) {
        use std::os::unix::fs::PermissionsExt;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("mkdir: {e}"));
        }
        std::fs::write(path, body).unwrap_or_else(|e| panic!("write: {e}"));
        if executable {
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).unwrap();
        }
    }

    /// A working proton install dir.
    fn proton_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join("runtime/proton").join(name);
        write_file(&dir.join("proton"), b"#!/bin/sh\nexit 0\n", true);
        dir
    }

    /// A steam compat dir with a proton install inside.
    fn steam_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join("steam/compatibilitytools.d").join(name);
        write_file(&dir.join("proton"), b"#!/bin/sh\nexit 0\n", true);
        dir
    }

    #[test]
    fn a_configured_path_wins() {
        let root = home("configured");
        let configured = proton_dir(&root, "GE-Proton9-1");
        let spec = RunnerSpec::with_configured(
            RunnerFamily::Proton,
            ConfiguredRunner::Path(configured.clone()),
        );
        let provider = ProtonProvider::with_roots(root.join("runtime"), vec![]);
        let resolved = provider.resolve(&spec).expect("resolve");
        let RunnerInstall::Discovered { path, version } = &resolved.reference.install else {
            panic!("configured paths are discover-only");
        };
        assert_eq!(path, &configured);
        assert_eq!(version, &None);
    }

    #[test]
    fn a_broken_configured_path_falls_through_to_managed() {
        let root = home("broken-config");
        let broken = root.join("configured");
        write_file(&broken.join("proton"), b"plain text", false);
        proton_dir(&root, "GE-Proton11-5");
        let spec =
            RunnerSpec::with_configured(RunnerFamily::Proton, ConfiguredRunner::Path(broken));
        let provider = ProtonProvider::with_roots(root.join("runtime"), vec![]);
        let resolved = provider.resolve(&spec).expect("falls through");
        assert_eq!(resolved.mode, ProviderMode::Managed);
        let RunnerInstall::Managed { path, version } = &resolved.reference.install else {
            panic!("managed");
        };
        assert_eq!(path, &root.join("runtime/proton/GE-Proton11-5"));
        assert_eq!(version.as_str(), "GE-Proton11-5");
    }

    #[test]
    fn a_version_pin_resolves_the_exact_managed_install() {
        let root = home("pin");
        proton_dir(&root, "GE-Proton9-1");
        proton_dir(&root, "GE-Proton11-5");
        let spec = RunnerSpec::with_configured(
            RunnerFamily::Proton,
            ConfiguredRunner::Version("GE-Proton9-1".to_owned()),
        );
        let provider = ProtonProvider::with_roots(root.join("runtime"), vec![]);
        let resolved = provider.resolve(&spec).expect("pinned");
        assert_eq!(resolved.mode, ProviderMode::Managed);
        let RunnerInstall::Managed { path, version } = &resolved.reference.install else {
            panic!("managed");
        };
        assert_eq!(path, &root.join("runtime/proton/GE-Proton9-1"));
        assert_eq!(version.as_str(), "GE-Proton9-1");
    }

    #[test]
    fn a_missing_version_pin_is_not_installed_not_unresolvable() {
        let root = home("pin-missing");
        let spec = RunnerSpec::with_configured(
            RunnerFamily::Proton,
            ConfiguredRunner::Version("GE-Proton9-1".to_owned()),
        );
        let provider = ProtonProvider::with_roots(root.join("runtime"), vec![]);
        assert_eq!(
            provider.resolve(&spec).expect_err("not installed"),
            ResolveError::NotInstalled {
                family: RunnerFamily::Proton
            },
            "a pin never falls through to discovery — SuggestInstall names the version"
        );
    }

    #[test]
    fn without_a_pin_the_newest_managed_install_wins() {
        let root = home("newest");
        let old = proton_dir(&root, "GE-Proton9-1");
        let new = proton_dir(&root, "GE-Proton11-5");
        // Explicit distinct mtimes make "newest" deterministic (install
        // order is what the mtime records).
        let set_mtime = |path: &Path, seconds: u64| {
            let times = std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds));
            std::fs::File::options()
                .write(true)
                .open(path)
                .and_then(|file| file.set_times(times))
                .unwrap();
        };
        set_mtime(&new.join("proton"), 2_000_000_000);
        set_mtime(&old.join("proton"), 1_000_000_000);
        let provider = ProtonProvider::with_roots(root.join("runtime"), vec![]);
        let resolved = provider
            .resolve(&RunnerSpec::new(RunnerFamily::Proton))
            .expect("newest resolves");
        assert_eq!(resolved.mode, ProviderMode::Managed);
        let RunnerInstall::Managed { path, version } = &resolved.reference.install else {
            panic!("managed");
        };
        assert_eq!(path, &new, "the newer install wins");
        assert_eq!(version.as_str(), "GE-Proton11-5");
    }

    #[test]
    fn manage_installs_land_on_top_of_steam_discovery() {
        let root = home("managed-over-steam");
        steam_dir(&root, "GE-Proton8-20");
        proton_dir(&root, "GE-Proton11-5");
        let steam_roots = vec![root.join("steam/compatibilitytools.d")];
        let provider = ProtonProvider::with_roots(root.join("runtime"), steam_roots);
        let resolved = provider
            .resolve(&RunnerSpec::new(RunnerFamily::Proton))
            .expect("managed wins");
        assert_eq!(resolved.mode, ProviderMode::Managed);
        let RunnerInstall::Managed { path, .. } = &resolved.reference.install else {
            panic!("managed");
        };
        assert_eq!(
            path,
            &root.join("runtime/proton/GE-Proton11-5"),
            "the resolution order is configured → managed → steam"
        );
    }

    #[test]
    fn steam_discovery_is_the_read_only_last_resort() {
        let root = home("steam-only");
        steam_dir(&root, "GE-Proton8-20");
        let steam_roots = vec![root.join("steam/compatibilitytools.d")];
        let provider = ProtonProvider::with_roots(root.join("runtime"), steam_roots);
        let resolved = provider
            .resolve(&RunnerSpec::new(RunnerFamily::Proton))
            .expect("steam discovery");
        assert_eq!(resolved.mode, ProviderMode::DiscoverOnly);
        let RunnerInstall::Discovered { path, version } = &resolved.reference.install else {
            panic!("discovered");
        };
        assert_eq!(path, &root.join("steam/compatibilitytools.d/GE-Proton8-20"));
        assert_eq!(version.as_deref(), Some("GE-Proton8-20"));
    }

    #[test]
    fn steam_scan_is_deterministic_and_read_only() {
        let root = home("scan");
        steam_dir(&root, "beta");
        steam_dir(&root, "GE-Proton11-5");
        steam_dir(&root, "alpha");
        // A non-proton dir is skipped.
        write_file(
            &root.join("steam/compatibilitytools.d/not-proton/garbage"),
            b"x",
            false,
        );
        let scan = ProtonProvider::scan_steam(&[root.join("steam/compatibilitytools.d")]);
        let versions: Vec<&str> = scan.iter().map(|proton| proton.version.as_str()).collect();
        assert_eq!(
            versions,
            ["GE-Proton11-5", "alpha", "beta"],
            "name-sorted, non-Proton dirs skipped"
        );
    }

    #[test]
    fn steam_common_proton_dirs_are_discovered() {
        let root = home("common");
        let dir = root.join("steam/steamapps/common/Proton 9.0");
        write_file(&dir.join("proton"), b"#!/bin/sh\nexit 0\n", true);
        let scan = ProtonProvider::scan_steam(&[root.join("steam/steamapps/common")]);
        assert_eq!(scan.len(), 1);
        assert_eq!(scan[0].version, "Proton 9.0");
    }

    #[test]
    fn other_families_are_not_serviced() {
        let provider = ProtonProvider::with_roots(PathBuf::from("/nope"), vec![]);
        let err = provider
            .resolve(&RunnerSpec::new(RunnerFamily::Wine))
            .expect_err("proton does not service wine");
        assert_eq!(err.family(), RunnerFamily::Proton);
    }

    #[test]
    fn nothing_at_all_is_unresolvable_with_a_suggestion() {
        let root = home("nothing");
        let provider = ProtonProvider::with_roots(root.join("runtime"), vec![]);
        assert_eq!(
            provider.resolve(&RunnerSpec::new(RunnerFamily::Proton)),
            Err(ResolveError::Unresolvable {
                family: RunnerFamily::Proton
            })
        );
    }
}
