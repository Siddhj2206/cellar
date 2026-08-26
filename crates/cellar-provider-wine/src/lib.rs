//! The discover-only wine provider (blueprint §5, §7): plain system wine
//! found via PATH with `execvp` semantics — read-only, never modified by
//! Cellar, and never a fallback for a failed Proton selection (research
//! #18).
//!
//! Implements only `RunnerResolver`; mode is trait membership, so there is
//! deliberately no `ManagedRunner` stub. Resolution follows the locked
//! order — configured path → managed → PATH — with the managed stage absent
//! by construction: a discover-only provider has no inventory of its own.
//! A broken configured path falls through to PATH rather than failing fast:
//! the locked wording is an *order*, not a hard stop. The resolved path is
//! canonicalized so the plan carries the real binary — reproducible and
//! exec-ready. No probing (`wine --version` would spawn; this slice's
//! dry-run is spawn-free — versions land with the execute slice #29).

use cellar_core::errors::{ResolveError, UnresolvedCause};
use cellar_core::ports::{__sealed, RunnerResolver};
use cellar_core::types::{
    ConfiguredRunner, ProviderMode, ResolvedRunner, RunnerFamily, RunnerInstall, RunnerRef,
    RunnerSpec,
};

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Discover-only wine provider.
#[derive(Debug, Default)]
pub struct WineProvider;

impl __sealed::Sealed for WineProvider {}

impl RunnerResolver for WineProvider {
    fn id(&self) -> &'static str {
        "wine"
    }

    fn resolve(&self, spec: &RunnerSpec) -> Result<ResolvedRunner, ResolveError> {
        Self::resolve_inner(spec, std::env::var_os("PATH").as_deref())
    }
}

impl WineProvider {
    /// The resolution stages with the PATH environment injected — the
    /// seam that keeps discovery testable without mutating the process
    /// environment (edition-2024 `set_var` is unsafe; the workspace
    /// forbids unsafe).
    fn resolve_inner(
        spec: &RunnerSpec,
        path: Option<&OsStr>,
    ) -> Result<ResolvedRunner, ResolveError> {
        if spec.family != RunnerFamily::Wine {
            // Not ours: report the provider's own family so a composition
            // root can tell "not me" from "serviced and exhausted" (the
            // family of the failure names what a SuggestInstall must find).
            return Err(ResolveError::Unresolvable {
                family: RunnerFamily::Wine,
                cause: UnresolvedCause::NoneFound {
                    mode: ProviderMode::DiscoverOnly,
                },
            });
        }
        // Stage 1 — a configured path wins (resolution prefers a configured
        // path over PATH). A path that is not an executable file falls
        // through to the next stage — the locked order (research #18) is a
        // sequence, not fail-fast; a stale configured path remains the
        // doctor's runner-integrity concern (#35), and when PATH also finds
        // nothing the miss is remembered as the failure's cause so the fix
        // hint names the stale path instead of a host install.
        let mut stale_configured = None;
        if let Some(configured) = &spec.configured {
            match configured {
                ConfiguredRunner::Path(path) if executable_file(path) => {
                    return Ok(resolved(wine_ref(path)));
                }
                ConfiguredRunner::Path(path) => stale_configured = Some(path.clone()),
                // A version pin asks for a managed install; wine has none.
                ConfiguredRunner::Version(_) => {
                    return Err(ResolveError::Unresolvable {
                        family: RunnerFamily::Wine,
                        cause: UnresolvedCause::NoneFound {
                            mode: ProviderMode::DiscoverOnly,
                        },
                    });
                }
            }
        }
        // Stage 3 — PATH, `execvp` semantics (the managed stage is absent
        // by trait membership).
        match find_on_path(path) {
            Some(path) => Ok(resolved(wine_ref(&path))),
            None => Err(ResolveError::Unresolvable {
                family: RunnerFamily::Wine,
                cause: match stale_configured {
                    Some(path) => UnresolvedCause::StaleConfigured { path },
                    None => UnresolvedCause::NoneFound {
                        mode: ProviderMode::DiscoverOnly,
                    },
                },
            }),
        }
    }
}

fn wine_ref(path: &Path) -> RunnerRef {
    RunnerRef {
        provider_id: "wine".to_owned(),
        family: RunnerFamily::Wine,
        install: RunnerInstall::Discovered {
            path: path.to_path_buf(),
            version: None,
        },
    }
}

fn resolved(reference: RunnerRef) -> ResolvedRunner {
    ResolvedRunner {
        mode: ProviderMode::DiscoverOnly,
        reference,
    }
}

/// POSIX `execvp` command lookup for `wine`, over the shared predicate
/// (#52): the directories of `PATH` are searched in order; an empty
/// component is the current directory; the first entry that is an
/// executable regular file wins, canonicalized — the plan carries the
/// real binary.
fn find_on_path(path: Option<&OsStr>) -> Option<PathBuf> {
    cellar_core::exec_lookup::find_on_path_in("wine", path)
}

fn executable_file(path: &Path) -> bool {
    cellar_core::exec_lookup::executable_file(path)
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::{WineProvider, executable_file, find_on_path};

    use cellar_core::errors::{ResolveError, UnresolvedCause};
    use cellar_core::ports::RunnerResolver;
    use cellar_core::types::{
        ConfiguredRunner, ProviderMode, RunnerFamily, RunnerInstall, RunnerSpec,
    };

    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("cellar-wine-{}-{seq}", std::process::id()))
    }

    /// A fresh directory containing an executable stub named `wine`.
    fn wine_dir() -> PathBuf {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {dir:?}: {e}"));
        let wine = dir.join("wine");
        std::fs::write(&wine, "#!/bin/sh\nexit 0\n").unwrap_or_else(|e| panic!("write: {e}"));
        let mut perms = std::fs::metadata(&wine)
            .unwrap_or_else(|e| panic!("meta: {e}"))
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&wine, perms).unwrap_or_else(|e| panic!("chmod: {e}"));
        dir
    }

    fn join_path(dirs: &[&Path]) -> String {
        dirs.iter()
            .map(|dir| dir.display().to_string())
            .collect::<Vec<_>>()
            .join(":")
    }

    #[test]
    fn finds_the_first_executable_wine_on_path() {
        let first = wine_dir();
        let second = wine_dir();
        let path = join_path(&[&first, &second]);
        let found =
            find_on_path(Some(std::ffi::OsStr::new(&path))).unwrap_or_else(|| panic!("{path}"));
        assert_eq!(
            found,
            first
                .join("wine")
                .canonicalize()
                .unwrap_or_else(|e| panic!("canon: {e}")),
            "the first PATH dir containing an executable wine wins"
        );
    }

    #[test]
    fn skips_a_non_executable_wine_and_keeps_searching() {
        let first = temp_dir();
        std::fs::create_dir_all(&first).unwrap();
        std::fs::write(first.join("wine"), "plain text").unwrap();
        let second = wine_dir();
        let found = find_on_path(Some(std::ffi::OsStr::new(&join_path(&[&first, &second]))));
        assert_eq!(
            found,
            Some(second.join("wine").canonicalize().unwrap()),
            "an uninvocable wine is not a match; the search continues"
        );
    }

    #[test]
    fn skips_a_directory_named_wine() {
        let first = temp_dir();
        std::fs::create_dir_all(first.join("wine")).unwrap();
        let second = wine_dir();
        let found = find_on_path(Some(std::ffi::OsStr::new(&join_path(&[&first, &second]))));
        assert_eq!(
            found,
            Some(second.join("wine").canonicalize().unwrap()),
            "a directory named wine is not a binary"
        );
    }

    #[test]
    fn empty_path_entries_do_not_break_the_order() {
        let first = wine_dir();
        let second = wine_dir();
        let path = format!("{}::{}", first.display(), second.display());
        let found = find_on_path(Some(std::ffi::OsStr::new(&path)));
        assert_eq!(
            found,
            Some(first.join("wine").canonicalize().unwrap()),
            "an empty entry (the current directory) keeps the search order"
        );
    }

    #[test]
    fn a_missing_path_finds_nothing_execvp_style() {
        assert_eq!(find_on_path(None), None, "no PATH, no lookup");
        assert_eq!(
            find_on_path(Some(std::ffi::OsStr::new("/nonexistent-dir"))),
            None
        );
    }

    #[test]
    fn a_configured_path_wins_over_path() {
        let configured = wine_dir().join("wine");
        let spec = RunnerSpec::with_configured(
            RunnerFamily::Wine,
            ConfiguredRunner::Path(configured.clone()),
        );
        let provider = WineProvider;
        let resolved = provider
            .resolve(&spec)
            .unwrap_or_else(|e| panic!("resolve: {e}"));
        assert_eq!(resolved.mode, ProviderMode::DiscoverOnly);
        let RunnerInstall::Discovered { path, .. } = &resolved.reference.install else {
            panic!("discover-only resolution must return a discovered install");
        };
        assert_eq!(path, &configured, "the configured path beats PATH");
    }

    #[test]
    fn a_broken_configured_path_falls_through_to_path() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let configured = dir.join("wine");
        std::fs::write(&configured, "not executable").unwrap();
        let path_dir = wine_dir();
        let spec =
            RunnerSpec::with_configured(RunnerFamily::Wine, ConfiguredRunner::Path(configured));
        let path = join_path(&[&path_dir]);
        let resolved = WineProvider::resolve_inner(&spec, Some(std::ffi::OsStr::new(&path)))
            .unwrap_or_else(|e| panic!("resolve: {e}"));
        let RunnerInstall::Discovered { path, .. } = &resolved.reference.install else {
            panic!("discovered install expected");
        };
        assert_eq!(
            path,
            &path_dir.join("wine").canonicalize().unwrap(),
            "the PATH discovery follows the configured-path stage"
        );
    }

    #[test]
    fn a_broken_configured_path_names_itself_when_path_also_misses() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let configured = dir.join("wine");
        std::fs::write(&configured, "not executable").unwrap();
        let spec = RunnerSpec::with_configured(
            RunnerFamily::Wine,
            ConfiguredRunner::Path(configured.clone()),
        );
        let err =
            WineProvider::resolve_inner(&spec, Some(std::ffi::OsStr::new("/nonexistent-dir")))
                .expect_err("no wine anywhere");
        assert_eq!(
            err,
            ResolveError::Unresolvable {
                family: RunnerFamily::Wine,
                cause: UnresolvedCause::StaleConfigured { path: configured },
            },
            "the stale path is named exactly — the doctor's hint points at it"
        );
    }

    #[test]
    fn a_version_pin_is_unresolvable_for_discover_only_wine() {
        let spec = RunnerSpec::with_configured(
            RunnerFamily::Wine,
            ConfiguredRunner::Version("9.0".to_owned()),
        );
        let err = WineProvider.resolve(&spec).expect_err("no managed wine");
        assert_eq!(
            err,
            ResolveError::Unresolvable {
                family: RunnerFamily::Wine,
                cause: UnresolvedCause::NoneFound {
                    mode: ProviderMode::DiscoverOnly,
                },
            }
        );
    }

    #[test]
    fn other_families_are_not_serviced() {
        let provider = WineProvider;
        let err = provider
            .resolve(&RunnerSpec::new(RunnerFamily::Proton))
            .expect_err("wine does not service proton");
        assert_eq!(
            err.family(),
            RunnerFamily::Wine,
            "the failure names the provider's own family, not the caller's"
        );
    }

    #[test]
    fn executable_file_is_a_regular_executable() {
        let dir = wine_dir();
        assert!(executable_file(&dir.join("wine")));
        assert!(!executable_file(&dir), "a directory is not executable_file");
        assert!(
            !executable_file(&dir.join("missing")),
            "absence is not a match"
        );
    }
}
