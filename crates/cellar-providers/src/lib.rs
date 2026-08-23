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

use cellar_core::entities::Prefix;
use cellar_core::ports::{ManagedRunner, RunnerResolver, WrapperContributor};
use cellar_core::types::{ResolvedRunner, RunnerFamily};
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
) -> Vec<Box<dyn WrapperContributor>> {
    let mut wrappers: Vec<Box<dyn WrapperContributor>> = Vec::new();
    if prefix.defaults.graphics.as_deref() == Some("gamescope") {
        wrappers.push(Box::new(GamescopeProvider));
    }
    if let (RunnerFamily::Proton, Some(umu)) = (resolved.reference.family, umu_run) {
        wrappers.push(Box::new(UmuWrapper::new(
            umu, resolved, prefix_dir, game_id,
        )));
    }
    wrappers
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
    let data = match std::env::var("XDG_DATA_HOME") {
        Ok(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::env::var_os("HOME").map_or_else(
            || std::path::PathBuf::from("."),
            |home| std::path::PathBuf::from(home).join(".local").join("share"),
        ),
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

    use cellar_core::entities::{Prefix, PrefixDefaults};
    use cellar_core::types::{Layer, ProviderMode, RunnerFamily, RunnerInstall, RunnerRef};
    use cellar_provider_umu::install_path;

    use std::path::{Path, PathBuf};

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
        );
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
        let wrappers = wrappers_for(&wine, None, &prefix, Path::new("/p"), "umu-x");
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
        let wrappers = wrappers_for(&wine, None, &prefix, Path::new("/p"), "umu-x");
        assert_eq!(wrappers.len(), 1);
        assert_eq!(wrappers[0].layer(), Layer::Display);
    }
}
