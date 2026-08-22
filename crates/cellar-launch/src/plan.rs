//! The pure plan rules (blueprint §7): the selection walk, wrapper-chain
//! assembly, and env merging. Every function is a pure value transformation
//! — same inputs → same plan — which is what makes `--dry-run` and the GUI
//! preview free features and the printed plan a reproducible bug report.
//!
//! Env contracts are wrapper-provider data, never launch machinery (ADR
//! 0003): the machinery only orders them. The one runner-side contract the
//! machinery knows is the bound prefix path in `WINEPREFIX` (plain wine's
//! launch contract — what makes a launch use *its* prefix).

use cellar_core::entities::{AppEntry, Prefix, Settings};
use cellar_core::ports::WrapperContributor;
use cellar_core::types::{LaunchPlan, ResolvedRunner, RunnerFamily, RunnerInstall, RunnerSpec};

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::LaunchError;

/// Two-stage precedence, selection stage (blueprint §7): app override →
/// prefix default → defaults floor. The floor is kind-preset by default —
/// Game → Proton (GE-Proton), Tool → wine (glossary: kind) — with a
/// nonempty `settings.resolution_order` replacing the preset: settings win,
/// presets fall back (wine is never an *automatic* fallback, so no chain is
/// consulted either).
///
/// The prefix-binding override (glossary: Override) has already decided
/// *which* prefix's defaults apply — orchestration passes the bound prefix
/// in (#28).
pub fn select_spec(entry: &AppEntry, prefix: &Prefix, settings: &Settings) -> RunnerSpec {
    if let Some(spec) = &entry.overrides.runner {
        return spec.clone();
    }
    if let Some(spec) = &prefix.defaults.runner {
        return spec.clone();
    }
    let family = settings
        .resolution_order
        .first()
        .copied()
        .unwrap_or_else(|| entry.kind.default_family());
    RunnerSpec::new(family)
}

/// Plan phase (blueprint §7): assemble the frozen plan — a pure, printable
/// value — from the resolved pieces.
///
/// `argv`: the resolved runner's invocation, `<runner> <exe> [args…]` with
/// the resolved path as `argv[0]` (exec-ready, reproduction-ready). Plain
/// wine is the planable family this slice; Proton-family plans are
/// unreachable until the umu container chain lands (#34) and fail loudly
/// rather than guess a chain.
///
/// `env`: four rungs, highest precedence last — base (the runner's own
/// contract: wine must use the bound prefix, `WINEPREFIX`) ← prefix env ←
/// wrapper contributions (in `Layer` order) ← app env overrides.
pub fn build_plan(
    entry: &AppEntry,
    prefix: &Prefix,
    resolved: &ResolvedRunner,
    prefix_dir: &Path,
    wrappers: &[&dyn WrapperContributor],
    args: &[String],
) -> Result<LaunchPlan, LaunchError> {
    let (command, base_env) = match &resolved.reference.family {
        RunnerFamily::Wine => {
            let RunnerInstall::Discovered { path, .. } = &resolved.reference.install else {
                // Structural impossibility today (wine is discover-only by
                // trait membership); a loud error beats a silent guess.
                return Err(LaunchError::PlanUnavailable {
                    family: RunnerFamily::Wine,
                });
            };
            let mut command = vec![path.to_string_lossy().into_owned()];
            command.push(entry.exe.to_string_lossy().into_owned());
            command.extend(args.iter().cloned());
            let mut base_env = BTreeMap::new();
            base_env.insert(
                "WINEPREFIX".to_owned(),
                prefix_dir.to_string_lossy().into_owned(),
            );
            (command, base_env)
        }
        family => {
            return Err(LaunchError::PlanUnavailable { family: *family });
        }
    };
    // The plan starts at the base + prefix rungs; wrappers and overrides
    // land on it below, in precedence order.
    let mut plan = LaunchPlan {
        argv: command,
        env: base_env,
        cwd: None,
        wrappers: Vec::new(),
    };
    merge_env(&mut plan.env, &prefix.defaults.env);
    // Wrapper contributions rung — contributed env is wrapper data; the
    // chain is a generic builder sorted by `Layer` (ADR 0003).
    apply_wrappers(&mut plan, wrappers);
    // App overrides rung — the highest precedence.
    merge_env(&mut plan.env, &entry.overrides.env);
    Ok(plan)
}

/// Apply wrapper contributions in `Layer` order (outermost first: Display →
/// Container → `RuntimeEnv`; blueprint §5), recording which layers the plan
/// goes through. A generic chain builder: no wrapper is hard-coded here —
/// env contracts are wrapper-provider data. The effective chain this slice
/// is empty (wine plans); the activation rules (which wrappers a plan gets)
/// land with the managed pipeline (#34).
pub fn apply_wrappers(plan: &mut LaunchPlan, wrappers: &[&dyn WrapperContributor]) {
    let mut sorted: Vec<&dyn WrapperContributor> = wrappers.to_vec();
    sorted.sort_by_key(|wrapper| wrapper.layer());
    plan.wrappers = sorted.iter().map(|wrapper| wrapper.layer()).collect();
    for wrapper in sorted {
        wrapper.contribute(plan);
    }
}

/// Merge one rung over another: `higher`'s entries replace `lower`'s per
/// key (the later rung wins).
fn merge_env(lower: &mut BTreeMap<String, String>, higher: &BTreeMap<String, String>) {
    for (key, value) in higher {
        lower.insert(key.clone(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_wrappers, build_plan, select_spec};

    use crate::LaunchError;

    use cellar_core::entities::{AppEntry, AppKind, Overrides, Prefix, PrefixDefaults, Settings};
    use cellar_core::ports::{__sealed, WrapperContributor};
    use cellar_core::types::{
        ConfiguredRunner, LaunchPlan, Layer, ProviderMode, ResolvedRunner, RunnerFamily,
        RunnerInstall, RunnerRef, RunnerSpec,
    };

    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, PoisonError};

    fn entry(kind: AppKind) -> AppEntry {
        AppEntry {
            slug: "balatro".to_owned(),
            exe: PathBuf::from("/games/balatro.exe"),
            kind,
            prefix: "default".to_owned(),
            overrides: Overrides::default(),
            runner: None,
            source_installer: None,
            installed_at: None,
        }
    }

    fn prefix(runner: Option<RunnerSpec>) -> Prefix {
        Prefix {
            slug: "default".to_owned(),
            defaults: PrefixDefaults {
                runner,
                ..PrefixDefaults::default()
            },
        }
    }

    fn wine_resolved() -> ResolvedRunner {
        ResolvedRunner {
            mode: ProviderMode::DiscoverOnly,
            reference: RunnerRef {
                provider_id: "wine".to_owned(),
                family: RunnerFamily::Wine,
                install: RunnerInstall::Discovered {
                    path: PathBuf::from("/usr/bin/wine"),
                    version: None,
                },
            },
        }
    }

    #[test]
    fn selection_app_override_beats_prefix_default_and_floor() {
        let mut app = entry(AppKind::Game);
        app.overrides.runner = Some(RunnerSpec::new(RunnerFamily::Wine));
        let prefix = prefix(Some(RunnerSpec::new(RunnerFamily::Proton)));
        let spec = select_spec(&app, &prefix, &Settings::default());
        assert_eq!(spec, RunnerSpec::new(RunnerFamily::Wine));
    }

    #[test]
    fn selection_prefix_default_beats_the_kind_floor() {
        let app = entry(AppKind::Tool); // floor: wine
        let prefix = prefix(Some(RunnerSpec::new(RunnerFamily::Proton)));
        let spec = select_spec(&app, &prefix, &Settings::default());
        assert_eq!(spec, RunnerSpec::new(RunnerFamily::Proton));
    }

    #[test]
    fn selection_floor_is_the_kind_preset() {
        let default = prefix(None);
        assert_eq!(
            select_spec(&entry(AppKind::Game), &default, &Settings::default()),
            RunnerSpec::new(RunnerFamily::Proton),
            "games floor at GE-Proton"
        );
        assert_eq!(
            select_spec(&entry(AppKind::Tool), &default, &Settings::default()),
            RunnerSpec::new(RunnerFamily::Wine),
            "tools floor at wine"
        );
    }

    #[test]
    fn selection_settings_order_replaces_the_kind_floor() {
        let settings = cellar_core::Settings {
            resolution_order: vec![RunnerFamily::Wine],
        };
        let spec = select_spec(&entry(AppKind::Game), &prefix(None), &settings);
        assert_eq!(
            spec,
            RunnerSpec::new(RunnerFamily::Wine),
            "configured order wins over the kind preset at the floor"
        );
    }

    #[test]
    fn selection_keeps_the_overrides_configured_pin() {
        // An app override may carry its own pin; selection must not strip it
        // — the resolution stage (configured → managed → PATH) needs it.
        let mut app = entry(AppKind::Game);
        app.overrides.runner = Some(RunnerSpec::with_configured(
            RunnerFamily::Wine,
            ConfiguredRunner::Path(PathBuf::from("/opt/wine/bin/wine")),
        ));
        let spec = select_spec(&app, &prefix(None), &Settings::default());
        assert_eq!(
            spec.configured,
            Some(ConfiguredRunner::Path(PathBuf::from("/opt/wine/bin/wine")))
        );
    }

    #[test]
    fn build_plan_wine_argv_env_rungs_and_empty_chain() {
        let app = entry(AppKind::Tool);
        let mut prefix = prefix(None);
        prefix
            .defaults
            .env
            .insert("PREFIX_VAR".to_owned(), "from-prefix".to_owned());
        let mut overridden = app.clone();
        overridden
            .overrides
            .env
            .insert("PREFIX_VAR".to_owned(), "from-app".to_owned());
        overridden
            .overrides
            .env
            .insert("APP_VAR".to_owned(), "from-app".to_owned());
        let plan = build_plan(
            &overridden,
            &prefix,
            &wine_resolved(),
            Path::new("/root/prefixes/default"),
            &[],
            &["--fullscreen".to_owned()],
        )
        .unwrap_or_else(|e| panic!("plan: {e}"));
        assert_eq!(
            plan.argv,
            [
                "/usr/bin/wine".to_owned(),
                "/games/balatro.exe".to_owned(),
                "--fullscreen".to_owned(),
            ],
            "argv = resolved runner, exe, args"
        );
        let env: Vec<_> = plan
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            env,
            [
                ("APP_VAR", "from-app"),
                ("PREFIX_VAR", "from-app"),
                ("WINEPREFIX", "/root/prefixes/default"),
            ],
            "base (WINEPREFIX) ← prefix env ← app overrides, overrides winning"
        );
        assert!(plan.wrappers.is_empty(), "empty chain this slice");
        assert_eq!(plan.cwd, None);
    }

    #[test]
    fn build_plan_applied_wrappers_sorted_by_layer_with_overrides_last() {
        let app = entry(AppKind::Tool);
        let prefix = prefix(None);
        let log: Arc<Mutex<Vec<Layer>>> = Arc::new(Mutex::new(Vec::new()));
        let mut env = BTreeMap::new();
        env.insert("WRAPPED".to_owned(), "wrapper".to_owned());
        let display = RecordingWrapper {
            layer: Layer::Display,
            env: BTreeMap::new(),
            log: Arc::clone(&log),
        };
        let runtime = RecordingWrapper {
            layer: Layer::RuntimeEnv,
            env: env.clone(),
            log: Arc::clone(&log),
        };
        let wrappers: Vec<&dyn WrapperContributor> = vec![&runtime, &display];
        let mut overridden = app;
        overridden
            .overrides
            .env
            .insert("WRAPPED".to_owned(), "app".to_owned());
        let plan = build_plan(
            &overridden,
            &prefix,
            &wine_resolved(),
            Path::new("/root/prefixes/default"),
            &wrappers,
            &[],
        )
        .unwrap_or_else(|e| panic!("plan: {e}"));
        let order = log.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(
            &*order,
            &[Layer::Display, Layer::RuntimeEnv],
            "outermost layer contributes first"
        );
        assert_eq!(plan.wrappers, [Layer::Display, Layer::RuntimeEnv]);
        assert_eq!(
            plan.env.get("WRAPPED").map(String::as_str),
            Some("app"),
            "app overrides outrank wrapper contributions"
        );
    }

    #[test]
    fn build_plan_rejects_families_without_a_chain_yet() {
        let app = entry(AppKind::Game);
        let resolved = ResolvedRunner {
            mode: ProviderMode::Managed,
            reference: RunnerRef {
                provider_id: "proton".to_owned(),
                family: RunnerFamily::Proton,
                install: RunnerInstall::Managed {
                    version: "9.0-4".to_owned(),
                    path: PathBuf::from("/runtime/proton-9.0-4"),
                },
            },
        };
        let err = build_plan(
            &app,
            &prefix(None),
            &resolved,
            Path::new("/root/prefixes/default"),
            &[],
            &[],
        )
        .expect_err("proton plans are unreachable before the umu chain");
        assert_eq!(
            err,
            LaunchError::PlanUnavailable {
                family: RunnerFamily::Proton
            }
        );
    }

    #[test]
    fn apply_wrappers_records_layers_and_merges_contributed_env() {
        let mut plan = LaunchPlan {
            argv: vec!["wine".to_owned(), "x.exe".to_owned()],
            env: BTreeMap::new(),
            cwd: None,
            wrappers: Vec::new(),
        };
        let mut env = BTreeMap::new();
        env.insert("GAMEID".to_owned(), "umu-default".to_owned());
        let wrapper = RecordingWrapper {
            layer: Layer::Container,
            env,
            log: Arc::new(Mutex::new(Vec::new())),
        };
        let wrappers: [&dyn WrapperContributor; 1] = [&wrapper];
        apply_wrappers(&mut plan, &wrappers);
        assert_eq!(plan.wrappers, [Layer::Container]);
        assert_eq!(
            plan.env.get("GAMEID").map(String::as_str),
            Some("umu-default")
        );
    }

    #[test]
    fn build_plan_is_a_pure_function_of_its_inputs() {
        // The reproducibility criterion: same inputs → byte-identical plan.
        let app = entry(AppKind::Tool);
        let prefix = prefix(None);
        let wrappers: &[&dyn cellar_core::ports::WrapperContributor] = &[];
        let args = ["-x".to_owned()];
        let inputs = (
            &app,
            &prefix,
            &wine_resolved(),
            Path::new("/root/prefixes/default"),
            wrappers,
            &args,
        );
        let first = build_plan(inputs.0, inputs.1, inputs.2, inputs.3, inputs.4, inputs.5)
            .unwrap_or_else(|e| panic!("plan: {e}"));
        let second = build_plan(inputs.0, inputs.1, inputs.2, inputs.3, inputs.4, inputs.5)
            .unwrap_or_else(|e| panic!("plan: {e}"));
        assert_eq!(first, second);
    }

    /// A wrapper double: records its contribution order and merges canned
    /// env pairs into the plan.
    #[derive(Debug)]
    struct RecordingWrapper {
        layer: Layer,
        env: BTreeMap<String, String>,
        log: Arc<Mutex<Vec<Layer>>>,
    }

    impl __sealed::Sealed for RecordingWrapper {}

    impl cellar_core::ports::WrapperContributor for RecordingWrapper {
        fn layer(&self) -> Layer {
            self.layer
        }

        fn contribute(&self, plan: &mut LaunchPlan) {
            self.log
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(self.layer);
            for (key, value) in &self.env {
                plan.env.insert(key.clone(), value.clone());
            }
        }
    }

    #[test]
    fn selection_pins_the_locked_walk_order() {
        // Compile-time guard: `select_spec` order is the locked walk — the
        // two `if let` rungs before the floor. This test pins the winners at
        // each rung so a reordering cannot silently change precedence.
        let mut app = entry(AppKind::Tool);
        app.overrides.runner = Some(RunnerSpec::new(RunnerFamily::Proton));
        let prefix = prefix(Some(RunnerSpec::new(RunnerFamily::Wine)));
        assert_eq!(
            select_spec(&app, &prefix, &Settings::default()),
            RunnerSpec::new(RunnerFamily::Proton),
            "override wins over prefix default"
        );
        app.overrides.runner = None;
        assert_eq!(
            select_spec(&app, &prefix, &Settings::default()),
            RunnerSpec::new(RunnerFamily::Wine),
            "prefix default wins over the tool floor"
        );
    }
}
