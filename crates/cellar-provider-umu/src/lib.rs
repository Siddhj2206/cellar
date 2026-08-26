//! The umu provider (blueprint §5, §7): the managed container launch layer.
//!
//! Implements the full triple seam: `RunnerResolver` (configured path →
//! managed install → PATH for the `umu-run` binary, research #18),
//! `ManagedRunner` (its zipapp release shape), and — via the
//! state-carrying [`UmuWrapper`] — `WrapperContributor` at
//! `Layer::Container`, contributing the umu env contract
//! (`GAMEID`/`WINEPREFIX`/`PROTONPATH`/`PROTON_VERB`) and prepending the
//! resolved `umu-run` binary: Cellar delegates, umu-run itself runs the
//! SLR container internally and appends its `_v2-entry-point → proton
//! waitforexitandrun` chain (research #18) — nothing in a Cellar plan
//! fabricates that expansion.

use cellar_core::errors::{ResolveError, UnresolvedCause};
use cellar_core::manifest::{
    ArchiveLayout, ChecksumScheme, InstallKind, ReleaseSource, RunnerManifest,
};
use cellar_core::ports::{__sealed, ManagedRunner, RunnerResolver, WrapperContributor};
use cellar_core::types::{
    ConfiguredRunner, LaunchPlan, Layer, ProviderMode, ResolvedRunner, RunnerFamily, RunnerInstall,
    RunnerRef, RunnerSpec,
};

use std::env;
use std::path::{Path, PathBuf};

/// Managed umu provider.
#[derive(Debug)]
pub struct UmuProvider {
    manifest: RunnerManifest,
    /// The tree's `runtime/` directory — the managed-install scan root
    /// (injected like the proton provider's; the provider never guesses a
    /// data root).
    runtime_dir: PathBuf,
}

impl UmuProvider {
    /// Stable provider identifier, shared by the resolver trait and the
    /// managed-runner manifest.
    pub const ID: &'static str = "umu";

    pub fn new() -> Self {
        Self::with_runtime(data_home().join("cellar").join("runtime"))
    }

    /// Over an explicit runtime dir (tests, embedded use).
    pub fn with_runtime(runtime_dir: PathBuf) -> Self {
        Self {
            manifest: RunnerManifest {
                provider_id: Self::ID.to_owned(),
                source: ReleaseSource {
                    url_template: "https://github.com/Open-Wine-Components/umu-launcher/releases/download/{tag}/umu-launcher-{tag}-zipapp.tar"
                        .to_owned(),
                    // The asset name/template is verified against release 1.4.4
                    // (2026-08); upstream publishes no checksum file for the
                    // zipapp — the pipeline installs it unverified (research
                    // #18; a truncated artifact still fails at extraction).
                    checksum_url_template: None,
                    latest_url: Some(
                        "https://github.com/Open-Wine-Components/umu-launcher/releases/latest"
                            .to_owned(),
                    ),
                },
                checksum: ChecksumScheme::Sha512,
                archive: ArchiveLayout::ExtractsToSingleRootDir,
                install_kind: InstallKind::LauncherBinary,
            },
            runtime_dir,
        }
    }

    /// Whether a directory holds a working umu install — the managed
    /// scan probe (an executable `umu-run` at the root).
    pub fn umu_dir_ok(dir: &Path) -> bool {
        executable_file(&dir.join("umu-run"))
    }
}

impl Default for UmuProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl __sealed::Sealed for UmuProvider {}

impl RunnerResolver for UmuProvider {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn resolve(&self, spec: &RunnerSpec) -> Result<ResolvedRunner, ResolveError> {
        Self::resolve_inner(spec, &self.runtime_dir, std::env::var_os("PATH").as_deref())
    }
}

impl UmuProvider {
    /// Resolution, research #18 order: configured path → managed install
    /// → PATH (`execvp` semantics, the wine provider's mechanism). A
    /// version pin never falls through — a pinned-but-missing managed
    /// install is `NotInstalled` (a `SuggestInstall` names the reinstall).
    fn resolve_inner(
        spec: &RunnerSpec,
        runtime_dir: &Path,
        path: Option<&std::ffi::OsStr>,
    ) -> Result<ResolvedRunner, ResolveError> {
        if spec.family != RunnerFamily::Umu {
            return Err(ResolveError::Unresolvable {
                family: RunnerFamily::Umu,
                cause: UnresolvedCause::NoneFound {
                    mode: ProviderMode::Managed,
                },
            });
        }
        if let Some(configured) = &spec.configured {
            match configured {
                ConfiguredRunner::Path(binary) if executable_file(binary) => {
                    return Ok(resolved(umu_ref(binary, None)));
                }
                ConfiguredRunner::Path(_) => {}
                ConfiguredRunner::Version(version) => {
                    let install = runtime_dir.join("umu").join(version);
                    if Self::umu_dir_ok(&install) {
                        let binary = install.join("umu-run");
                        return Ok(managed_resolved(&binary, version));
                    }
                    return Err(ResolveError::NotInstalled {
                        family: RunnerFamily::Umu,
                    });
                }
            }
        }
        // The managed stage: installed versions under the tree's runtime
        // dir, newest first (the proton provider's scan). Runtime-tree
        // installs resolve Managed — the mode tag rides the result
        // (blueprint §5).
        if let Some((version, install)) =
            newest_installed(&runtime_dir.join("umu"), Self::umu_dir_ok)
        {
            let binary = install.join("umu-run");
            return Ok(managed_resolved(&binary, &version));
        }
        // The discover-only stage: a distro-provided umu-run on PATH.
        if let Some(binary) = find_on_path(path) {
            return Ok(resolved(umu_ref(&binary, None)));
        }
        Err(ResolveError::Unresolvable {
            family: RunnerFamily::Umu,
            cause: UnresolvedCause::NoneFound {
                mode: ProviderMode::Managed,
            },
        })
    }
}

/// The umu container layer of one launch (blueprint §5): carries the
/// launch's concrete contract — the resolved `umu-run` binary, the bound
/// prefix (Cellar's own prefix, never umu's `$HOME` default), the resolved
/// Proton install, and the per-app `GAMEID` — and contributes it at
/// `Layer::Container`. The wrapper is provider data: the launch machinery
/// only orders it (ADR 0003).
#[derive(Debug, Clone)]
pub struct UmuWrapper {
    /// `argv[0]`: the resolved `umu-run` binary to delegate to.
    pub umu_run: PathBuf,
    /// `WINEPREFIX`: the bound prefix directory.
    pub prefix_dir: PathBuf,
    /// `PROTONPATH`: the resolved Proton install directory.
    pub proton_dir: PathBuf,
    /// `GAMEID`: the per-app protonfixes identity (`umu-<slug>`).
    pub game_id: String,
}

impl __sealed::Sealed for UmuWrapper {}

impl UmuWrapper {
    /// The wrapper for one managed-Proton launch over already-resolved
    /// state: the umu-run reference, the proton reference, the bound
    /// prefix directory, and the app identity.
    pub fn new(
        umu_run: &ResolvedRunner,
        proton: &ResolvedRunner,
        prefix_dir: &Path,
        game_id: &str,
    ) -> Self {
        Self {
            umu_run: install_path(&umu_run.reference.install).to_path_buf(),
            prefix_dir: prefix_dir.to_path_buf(),
            proton_dir: install_path(&proton.reference.install).to_path_buf(),
            game_id: game_id.to_owned(),
        }
    }
}

impl WrapperContributor for UmuWrapper {
    fn layer(&self) -> Layer {
        Layer::Container
    }

    fn contribute(&self, plan: &mut LaunchPlan) {
        // The delegation (research #18): Cellar spawns `umu-run` with the
        // exe; umu-run runs the SLR container internally and appends its
        // `_v2-entry-point → proton waitforexitandrun` chain. Prepending
        // here (the contribution runs innermost-first, so the Container
        // layer's prepend leaves outer layers outer).
        plan.argv
            .insert(0, self.umu_run.to_string_lossy().into_owned());
        plan.env.insert("GAMEID".to_owned(), self.game_id.clone());
        plan.env.insert(
            "WINEPREFIX".to_owned(),
            self.prefix_dir.to_string_lossy().into_owned(),
        );
        plan.env.insert(
            "PROTONPATH".to_owned(),
            self.proton_dir.to_string_lossy().into_owned(),
        );
        plan.env
            .insert("PROTON_VERB".to_owned(), "waitforexitandrun".to_owned());
    }
}

fn umu_ref(binary: &Path, version: Option<String>) -> RunnerRef {
    RunnerRef {
        provider_id: UmuProvider::ID.to_owned(),
        family: RunnerFamily::Umu,
        install: RunnerInstall::Discovered {
            path: binary.to_path_buf(),
            version,
        },
    }
}

/// A runtime-tree install resolves Managed — the mode tag rides the
/// result (blueprint §5).
fn managed_resolved(binary: &Path, version: &str) -> ResolvedRunner {
    ResolvedRunner {
        mode: ProviderMode::Managed,
        reference: RunnerRef {
            provider_id: UmuProvider::ID.to_owned(),
            family: RunnerFamily::Umu,
            install: RunnerInstall::Managed {
                version: version.to_owned(),
                path: binary.to_path_buf(),
            },
        },
    }
}

/// The concrete path of any resolved install (managed or discovered).
pub fn install_path(install: &RunnerInstall) -> &Path {
    match install {
        RunnerInstall::Managed { path, .. } | RunnerInstall::Discovered { path, .. } => path,
    }
}

fn resolved(reference: RunnerRef) -> ResolvedRunner {
    ResolvedRunner {
        mode: ProviderMode::DiscoverOnly,
        reference,
    }
}

/// The newest (by directory mtime) dir under `dir` whose `probe` passes,
/// as `(name, path)` — the same managed-install scan the proton provider
/// runs (kept per-provider: providers are independent crates).
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

/// POSIX `execvp` command lookup for `umu-run`: PATH directories searched
/// in order, the first executable regular file wins (the wine provider's
/// mechanism, mirrored here — providers are independent crates).
fn find_on_path(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path = path?;
    for dir in std::env::split_paths(path) {
        let candidate = dir.join("umu-run");
        if executable_file(&candidate) {
            return Some(candidate.canonicalize().unwrap_or(candidate));
        }
    }
    None
}

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

fn data_home() -> PathBuf {
    match env::var("XDG_DATA_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => env::var_os("HOME").map_or_else(
            || PathBuf::from("."),
            |home| PathBuf::from(home).join(".local").join("share"),
        ),
    }
}

impl ManagedRunner for UmuProvider {
    fn manifest(&self) -> &RunnerManifest {
        &self.manifest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn home(tag: &str) -> PathBuf {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("cellar-umu-{tag}-{}-{seq}", std::process::id()))
    }

    fn write_executable(path: &Path, body: &[u8]) {
        use std::os::unix::fs::PermissionsExt;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    /// A working managed umu install.
    fn umu_dir(root: &Path, version: &str) -> PathBuf {
        let dir = root.join("runtime/umu").join(version);
        write_executable(&dir.join("umu-run"), b"#!/bin/sh\nexit 0\n");
        dir
    }

    fn join_path(dirs: &[&Path]) -> String {
        dirs.iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(":")
    }

    #[test]
    fn a_configured_umu_path_wins() {
        let root = home("configured");
        let binary = root.join("bin/umu-run");
        write_executable(&binary, b"#!/bin/sh\nexit 0\n");
        let spec =
            RunnerSpec::with_configured(RunnerFamily::Umu, ConfiguredRunner::Path(binary.clone()));
        let provider = UmuProvider::with_runtime(root.join("runtime"));
        let resolved = provider.resolve(&spec).expect("resolve");
        let RunnerInstall::Discovered { path, .. } = &resolved.reference.install else {
            panic!("discovered");
        };
        assert_eq!(path, &binary);
    }

    #[test]
    fn a_version_pin_resolves_the_managed_install() {
        let root = home("pin");
        umu_dir(&root, "1.4.4");
        let spec = RunnerSpec::with_configured(
            RunnerFamily::Umu,
            ConfiguredRunner::Version("1.4.4".to_owned()),
        );
        let provider = UmuProvider::with_runtime(root.join("runtime"));
        let resolved = provider.resolve(&spec).expect("pinned");
        assert_eq!(resolved.mode, ProviderMode::Managed);
        let RunnerInstall::Managed { path, version } = &resolved.reference.install else {
            panic!("managed");
        };
        assert_eq!(path, &root.join("runtime/umu/1.4.4/umu-run"));
        assert_eq!(version.as_str(), "1.4.4");
    }

    #[test]
    fn a_missing_pin_is_not_installed() {
        let root = home("pin-missing");
        let spec = RunnerSpec::with_configured(
            RunnerFamily::Umu,
            ConfiguredRunner::Version("1.4.4".to_owned()),
        );
        let provider = UmuProvider::with_runtime(root.join("runtime"));
        assert_eq!(
            provider.resolve(&spec).expect_err("not installed"),
            ResolveError::NotInstalled {
                family: RunnerFamily::Umu
            }
        );
    }

    #[test]
    fn managed_install_wins_over_path_discovery() {
        let root = home("managed-first");
        umu_dir(&root, "1.4.4");
        let path_dir = root.join("bin");
        write_executable(&path_dir.join("umu-run"), b"#!/bin/sh\nexit 0\n");
        let provider = UmuProvider::with_runtime(root.join("runtime"));
        let resolved = provider
            .resolve(&RunnerSpec::new(RunnerFamily::Umu))
            .expect("managed");
        assert_eq!(resolved.mode, ProviderMode::Managed);
        let RunnerInstall::Managed { path, .. } = &resolved.reference.install else {
            panic!("managed");
        };
        assert_eq!(path, &root.join("runtime/umu/1.4.4/umu-run"));
    }

    #[test]
    fn path_discovery_is_the_last_stage() {
        let root = home("path");
        let path_dir = root.join("bin");
        write_executable(&path_dir.join("umu-run"), b"#!/bin/sh\nexit 0\n");
        let spec = RunnerSpec::new(RunnerFamily::Umu);
        let resolved = UmuProvider::resolve_inner(
            &spec,
            &root.join("runtime"),
            Some(std::ffi::OsStr::new(&join_path(&[&path_dir]))),
        )
        .expect("path stage");
        let RunnerInstall::Discovered { path, .. } = &resolved.reference.install else {
            panic!("discovered");
        };
        assert_eq!(path, &path_dir.join("umu-run").canonicalize().unwrap());
    }

    #[test]
    fn the_wrapper_contributes_the_env_contract_and_prepends_umu_run() {
        let mut plan = LaunchPlan {
            argv: vec!["/games/balatro.exe".to_owned(), "-x".to_owned()],
            env: BTreeMap::new(),
            cwd: None,
            wrappers: Vec::new(),
        };
        let wrapper = UmuWrapper {
            umu_run: PathBuf::from("/runtime/umu/1.4.4/umu-run"),
            prefix_dir: PathBuf::from("/root/prefixes/default"),
            proton_dir: PathBuf::from("/runtime/proton/GE-Proton11-5"),
            game_id: "umu-balatro".to_owned(),
        };
        wrapper.contribute(&mut plan);
        assert_eq!(
            plan.argv,
            [
                "/runtime/umu/1.4.4/umu-run".to_owned(),
                "/games/balatro.exe".to_owned(),
                "-x".to_owned(),
            ],
            "umu-run is prepended — the delegation (research #18)"
        );
        assert_eq!(
            plan.env.get("GAMEID").map(String::as_str),
            Some("umu-balatro")
        );
        assert_eq!(
            plan.env.get("WINEPREFIX").map(String::as_str),
            Some("/root/prefixes/default")
        );
        assert_eq!(
            plan.env.get("PROTONPATH").map(String::as_str),
            Some("/runtime/proton/GE-Proton11-5")
        );
        assert_eq!(
            plan.env.get("PROTON_VERB").map(String::as_str),
            Some("waitforexitandrun")
        );
        assert_eq!(wrapper.layer(), Layer::Container);
    }

    #[test]
    fn other_families_are_not_serviced() {
        let provider = UmuProvider::with_runtime(PathBuf::from("/nope"));
        let err = provider
            .resolve(&RunnerSpec::new(RunnerFamily::Wine))
            .expect_err("umu does not service wine");
        assert_eq!(err.family(), RunnerFamily::Umu);
    }

    #[test]
    fn nothing_at_all_is_unresolvable() {
        let root = home("nothing");
        let provider = UmuProvider::with_runtime(root.join("runtime"));
        assert_eq!(
            provider.resolve(&RunnerSpec::new(RunnerFamily::Umu)),
            Err(ResolveError::Unresolvable {
                family: RunnerFamily::Umu,
                cause: UnresolvedCause::NoneFound {
                    mode: ProviderMode::Managed,
                },
            })
        );
    }
}
