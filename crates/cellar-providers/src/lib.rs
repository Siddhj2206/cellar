//! The provider registry (blueprint §4): static, explicit wiring of the
//! provider crates, consumed by presentation crates only (composition root —
//! nothing below presentation constructs infra). The registry API is the
//! `all_*` functions below plus [`wrappers_for`], the per-launch wrapper
//! activation rule (research #17); presentations bind their results at the
//! composition root.
//!
//! Adding a provider = one new crate + one line here + one presentation dep —
//! zero edits below presentation (research #17: explicit registry over
//! link-time `inventory` magic).
//!
//! The blueprint's "provider crates only" line means the registry never
//! depends on `app`/`launch`/`storage`/`desktop`; it still depends on
//! `cellar-core`, whose trait types (`dyn RunnerResolver` & co.) are the
//! registry's return vocabulary (ADR 0002 amendment, 2026-08).

use cellar_core::entities::{GraphicsSelection, Prefix};
use cellar_core::exec_lookup::PathLookup;
use cellar_core::manifest::ManagedRecord;
use cellar_core::ports::{ManagedRunner, RunnerResolver, WrapperContributor};
use cellar_core::types::{MissingWrapper, ResolvedRunner, RunnerFamily};
use cellar_provider_gamescope::GamescopeProvider;
use cellar_provider_proton::{ProtonProvider, SteamProton};
use cellar_provider_umu::{UmuProvider, UmuWrapper};
use cellar_provider_wine::WineProvider;

use std::path::Path;

/// Every runner-resolving provider in the workspace, over the tree's
/// `runtime/` directory (the managed-install scan root — the composition
/// root passes `store.data_root()/runtime`; the providers never guess a
/// data root).
pub fn all_resolvers(runtime_dir: &Path) -> Vec<Box<dyn RunnerResolver>> {
    vec![
        Box::new(ProtonProvider::with_roots(
            runtime_dir.to_path_buf(),
            steam_roots(),
        )),
        Box::new(UmuProvider::with_runtime(runtime_dir.to_path_buf())),
        Box::new(WineProvider),
    ]
}

/// Every managed provider (drives the storage-owned installer pipeline).
pub fn all_managed() -> Vec<Box<dyn ManagedRunner>> {
    vec![
        Box::new(ProtonProvider::new()),
        Box::new(UmuProvider::new()),
    ]
}

/// The wrapper chain of one launch (blueprint §5 activation rules) — the
/// rule's home is the registry, next to the providers whose types it
/// names:
///
/// - the **Display** layer joins when the bound prefix's graphics default
///   selects gamescope (`prefix.toml` `graphics = "gamescope"`),
///   prepending the `gamescope` binary (PATH-resolved at spawn);
/// - the **Container** layer joins every Proton-family plan — managed
///   installs and Steam discoveries alike, since a Proton launch outside
///   the umu container is unsupported (research #18) — carrying the
///   resolved `umu-run` binary and the umu env contract
///   (`GAMEID`/`WINEPREFIX`/`PROTONPATH`/`PROTON_VERB`).
///
/// Returns wrappers in no particular order; launch sorts and applies them
/// by `Layer`.
pub fn wrappers_for(
    resolved: &ResolvedRunner,
    umu_run: Option<&ResolvedRunner>,
    prefix: &Prefix,
    prefix_dir: &Path,
    game_id: &str,
    lookup: PathLookup,
) -> Result<Vec<Box<dyn WrapperContributor>>, MissingWrapper> {
    let mut wrappers: Vec<Box<dyn WrapperContributor>> = Vec::new();
    // One parse point for `graphics` (#52): entities owns the vocabulary.
    // An unknown value warns-and-continues unwrapped — the warning is the
    // launch surface's (cellar-app); the registry only declines to wrap.
    match prefix.defaults.graphics_selection() {
        Some(GraphicsSelection::Gamescope) => {
            // The presence probe lives at activation (#52): a plan naming
            // `gamescope` argv[0] the host lacks would doctor false-pass
            // and die raw at exec. Failing here fails the plan stage,
            // which the doctor's plan section runs too.
            lookup("gamescope").ok_or(MissingWrapper {
                program: "gamescope",
            })?;
            wrappers.push(Box::new(GamescopeProvider));
        }
        Some(GraphicsSelection::Unrecognized(_)) | None => {}
    }
    if let (RunnerFamily::Proton, Some(umu)) = (resolved.reference.family, umu_run) {
        wrappers.push(Box::new(UmuWrapper::new(
            umu, resolved, prefix_dir, game_id,
        )));
    }
    Ok(wrappers)
}

/// The integrity probe for one recorded managed install: the same marker
/// the installer pipeline probes at install time — the provider's
/// launcher (`proton` script / `umu-run`). Read-only; the doctor's
/// composition root wires it through the app's `ManagedProbe` seam.
///
/// `None` means intact; `Some((problem, fix))` names the damage and its
/// fix in the provider's own vocabulary. An unknown provider id names
/// itself — a record Cellar cannot verify is never mis-attributed as a
/// broken install (review #35).
pub fn probe_managed(record: &ManagedRecord, runtime_dir: &Path) -> Option<(String, String)> {
    let install = runtime_dir.join(&record.install);
    let intact = match record.provider_id.as_str() {
        "proton" => ProtonProvider::proton_dir_ok(&install),
        "umu" => UmuProvider::umu_dir_ok(&install),
        _ => {
            return Some((
                "unknown provider — Cellar cannot verify this record".to_owned(),
                "no Cellar command installs it: remove the [[runner]] entry from \
                 runtime/providers.toml by hand, or check whether a future provider ships it"
                    .to_owned(),
            ));
        }
    };
    if intact {
        None
    } else {
        Some((
            "the install directory is missing, or its launcher is not executable".to_owned(),
            format!(
                "reinstall it: cellar runner install {} {}",
                record.provider_id, record.version
            ),
        ))
    }
}

/// Steam Proton discovery (read-only host state, research #18): every
/// working install under the env-derived Steam roots, name-sorted — the
/// discover-only enumeration `runner list` shows.
pub fn steam_protons() -> Vec<SteamProton> {
    ProtonProvider::scan_steam(&steam_roots())
}

/// The Steam compatibility-dir discovery roots (research #18: the
/// `compatibilitytools.d` layout and Steam's own `common/Proton *`
/// installs) derived from the environment — host state the proton
/// provider reads read-only.
pub fn steam_roots() -> Vec<std::path::PathBuf> {
    // The validated core resolution (#61): no $PWD-relative fallback.
    // When the environment is misconfigured the scan roots are simply
    // absent — the CLI's own store construction has already died with
    // exit 2 by the time anything reaches here.
    let Ok(data) = cellar_core::xdg::data_home() else {
        return Vec::new();
    };
    let mut roots = vec![data.join("Steam").join("compatibilitytools.d")];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(
            std::path::PathBuf::from(home)
                .join(".steam")
                .join("steam")
                .join("steamapps")
                .join("common"),
        );
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    use cellar_core::entities::PrefixDefaults;
    use cellar_core::types::{
        Layer, MissingWrapper, ProviderMode, RunnerFamily, RunnerInstall, RunnerRef,
    };
    use cellar_provider_umu::install_path;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Hermetic presence: gamescope "exists", nothing else does — the
    /// activation tests never touch the real PATH.
    fn fake_lookup(program: &str) -> Option<std::path::PathBuf> {
        (program == "gamescope").then(|| PathBuf::from("/usr/bin/gamescope"))
    }

    static SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn probe_managed_names_intact_broken_and_unknown_records() {
        use std::os::unix::fs::PermissionsExt;

        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let runtime = std::env::temp_dir().join(format!(
            "cellar-providers-probe-{}-{seq}",
            std::process::id()
        ));
        let intact = runtime.join("proton/GE-Proton11-5");
        std::fs::create_dir_all(&intact).unwrap();
        std::fs::write(intact.join("proton"), "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(intact.join("proton"))
            .unwrap()
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(intact.join("proton"), perms).unwrap();

        let record = |provider: &str| ManagedRecord {
            provider_id: provider.to_owned(),
            version: "GE-Proton11-5".to_owned(),
            install: format!("{provider}/GE-Proton11-5"),
        };
        // Intact: the launcher passes the probe.
        assert_eq!(probe_managed(&record("proton"), &runtime), None);
        // Broken: the record points at an install that is not there.
        let missing = probe_managed(&record("umu"), &runtime);
        assert!(missing.is_some());
        let (problem, fix) = missing.unwrap();
        assert!(problem.contains("launcher is not executable"), "{problem}");
        assert!(
            fix.contains("cellar runner install umu GE-Proton11-5"),
            "{fix}"
        );
        // Unknown: the record names itself — never mis-attributed damage.
        let unknown = probe_managed(&record("halfling"), &runtime).unwrap();
        assert!(
            unknown.0.contains("unknown provider"),
            "the record names itself: {}",
            unknown.0
        );
        assert!(
            !unknown.1.contains("cellar runner install halfling"),
            "the fix must not lie about a reinstall: {}",
            unknown.1
        );
    }

    #[test]
    fn registry_wires_every_provider() {
        let runtime = PathBuf::from("/tmp/registry-runtime");
        let resolver_ids: Vec<_> = all_resolvers(&runtime).iter().map(|r| r.id()).collect();
        assert_eq!(resolver_ids, ["proton", "umu", "wine"]);
        assert_eq!(all_managed().len(), 2);
    }

    #[test]
    fn wrapper_chain_layers_sort_from_the_registry() {
        let prefix = Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults {
                graphics: Some("gamescope".to_owned()),
                ..PrefixDefaults::default()
            },
        };
        let proton = ResolvedRunner {
            mode: ProviderMode::Managed,
            reference: RunnerRef {
                provider_id: "proton".to_owned(),
                family: RunnerFamily::Proton,
                install: RunnerInstall::Managed {
                    version: "GE-Proton11-5".to_owned(),
                    path: PathBuf::from("/runtime/proton/GE-Proton11-5"),
                },
            },
        };
        let umu = ResolvedRunner {
            mode: ProviderMode::Managed,
            reference: RunnerRef {
                provider_id: "umu".to_owned(),
                family: RunnerFamily::Umu,
                install: RunnerInstall::Managed {
                    version: "1.4.4".to_owned(),
                    path: PathBuf::from("/runtime/umu/1.4.4/umu-run"),
                },
            },
        };
        let wrappers = wrappers_for(
            &proton,
            Some(&umu),
            &prefix,
            Path::new("/root/prefixes/default"),
            "umu-balatro",
            fake_lookup,
        )
        .unwrap_or_else(|e| panic!("wrapper chain: {e:?}"));
        let mut layers: Vec<_> = wrappers.iter().map(|w| w.layer()).collect();
        layers.sort();
        assert_eq!(layers, [Layer::Display, Layer::Container]);
        assert_eq!(
            install_path(&umu.reference.install),
            Path::new("/runtime/umu/1.4.4/umu-run")
        );
        assert_eq!(
            install_path(&proton.reference.install),
            Path::new("/runtime/proton/GE-Proton11-5")
        );
    }

    #[test]
    fn plain_wine_plans_get_no_container_layer() {
        let prefix = Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults::default(),
        };
        let wine = ResolvedRunner {
            mode: ProviderMode::DiscoverOnly,
            reference: RunnerRef {
                provider_id: "wine".to_owned(),
                family: RunnerFamily::Wine,
                install: RunnerInstall::Discovered {
                    path: PathBuf::from("/usr/bin/wine"),
                    version: None,
                },
            },
        };
        let wrappers = wrappers_for(&wine, None, &prefix, Path::new("/p"), "umu-x", fake_lookup)
            .unwrap_or_else(|e| panic!("wrapper chain: {e:?}"));
        assert!(
            wrappers.is_empty(),
            "no wrappers for an unconfigured wine plan"
        );
    }

    #[test]
    fn gamescope_joins_other_families_when_configured() {
        let prefix = Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults {
                graphics: Some("gamescope".to_owned()),
                ..PrefixDefaults::default()
            },
        };
        let wine = ResolvedRunner {
            mode: ProviderMode::DiscoverOnly,
            reference: RunnerRef {
                provider_id: "wine".to_owned(),
                family: RunnerFamily::Wine,
                install: RunnerInstall::Discovered {
                    path: PathBuf::from("/usr/bin/wine"),
                    version: None,
                },
            },
        };
        let wrappers = wrappers_for(&wine, None, &prefix, Path::new("/p"), "umu-x", fake_lookup)
            .unwrap_or_else(|e| panic!("wrapper chain: {e:?}"));
        assert_eq!(wrappers.len(), 1);
        assert_eq!(wrappers[0].layer(), Layer::Display);
    }

    #[test]
    fn gamescope_missing_from_path_fails_the_activation() {
        // The presence probe at activation (#52): a plan naming gamescope
        // argv[0] the host lacks must fail here — where the doctor's plan
        // section sees it — never raw at exec.
        let prefix = Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults {
                graphics: Some("gamescope".to_owned()),
                ..PrefixDefaults::default()
            },
        };
        let wine = ResolvedRunner {
            mode: ProviderMode::DiscoverOnly,
            reference: RunnerRef {
                provider_id: "wine".to_owned(),
                family: RunnerFamily::Wine,
                install: RunnerInstall::Discovered {
                    path: PathBuf::from("/usr/bin/wine"),
                    version: None,
                },
            },
        };
        assert_eq!(
            wrappers_for(&wine, None, &prefix, Path::new("/p"), "umu-x", |_p| None).unwrap_err(),
            MissingWrapper {
                program: "gamescope"
            }
        );
    }

    #[test]
    fn an_unknown_graphics_value_never_wraps() {
        // Warn-and-continue (#52): the registry declines to wrap; the
        // warning is the launch surface's.
        let prefix = Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults {
                graphics: Some("gamescop".to_owned()),
                ..PrefixDefaults::default()
            },
        };
        let wine = ResolvedRunner {
            mode: ProviderMode::DiscoverOnly,
            reference: RunnerRef {
                provider_id: "wine".to_owned(),
                family: RunnerFamily::Wine,
                install: RunnerInstall::Discovered {
                    path: PathBuf::from("/usr/bin/wine"),
                    version: None,
                },
            },
        };
        let wrappers = wrappers_for(&wine, None, &prefix, Path::new("/p"), "umu-x", fake_lookup)
            .unwrap_or_else(|e| panic!("wrapper chain: {e:?}"));
        assert!(
            wrappers.is_empty(),
            "an unrecognized value must not silently wrap"
        );
    }
}
