//! The Cellar CLI — presentation, composition root (blueprint §8, ADR 0004).
//!
//! This binary is the *only* place concrete infra is instantiated: it builds
//! the tree adapter ([`TreeStore`]) and injects it into the application
//! services, which are generic over the `core` ports. Symmetric with the
//! future `cellar-gui` leaf; flipping primary is `default-members`, zero
//! edits below presentation.
//!
//! Surface for this slice (#29): `cellar launch <app>` executes the frozen
//! plan — foreground by default with the game's exit code propagated raw,
//! `--detach` releasing the process from the terminal, `-n/--dry-run`
//! printing the plan spawn-free — on top of #27's install/list/uninstall,
//! #26's `cellar prefix …` and `cellar doctor`. Exit codes (ADR 0004):
//! 0 success, 1 operation error or doctor problems, 2 usage; `launch`
//! propagates the game's exit code raw (§7), the collision with 1
//! documented, not mapped.

use clap::{Args, Parser, Subcommand};

use std::path::PathBuf;
use std::process::{ExitCode, ExitStatus};
use std::str::FromStr;

use cellar_app::{
    DoctorService, InstallService, LaunchApp, LaunchMode, ListedEntry, PrefixService,
};
use cellar_core::ports::{__sealed, RunnerResolver};
use cellar_core::{
    AppEntry, AppKind, LaunchPlan, Prefix, ResolveError, ResolvedRunner, RunnerRef, RunnerSpec,
    TreeHealth,
};
use cellar_providers::all_resolvers;
use cellar_storage::TreeStore;

/// The Cellar Windows app/game runtime for Linux.
#[derive(Debug, Parser)]
#[command(name = "cellar", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Register a standalone Windows exe without executing anything.
    Install(InstallArgs),
    /// List every registered app with its status.
    List(ListArgs),
    /// Launch a registered app through its launch plan — foreground by
    /// default with the exit code propagated raw, `--detach` to release
    /// the process from the terminal, `--dry-run` to preview spawn-free.
    Launch(LaunchArgs),
    /// Uninstall an app (removes its entry; app files stay on disk).
    Uninstall(UninstallArgs),
    /// Manage Cellar prefixes (blueprint §8: lifecycle objects get noun
    /// groups).
    Prefix(PrefixArgs),
    /// Sectioned capability checks with fix hints; exits 1 on any problem.
    Doctor(DoctorArgs),
}

#[derive(Debug, Args)]
struct InstallArgs {
    /// Path to the Windows executable to register.
    path: PathBuf,
    /// Prefix to bind the entry to; created when missing.
    #[arg(long, default_value = "default")]
    prefix: String,
    /// Display name for a new entry (default: the exe's file name).
    #[arg(long)]
    name: Option<String>,
    /// Entry kind — drives the defaults-floor preset hook (games → Proton,
    /// tools → wine).
    #[arg(long, value_parser = AppKind::from_str, default_value = "game")]
    kind: AppKind,
}

#[derive(Debug, Args)]
struct ListArgs {
    /// Machine-readable JSON output.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct UninstallArgs {
    /// The app slug, as shown by `list`.
    slug: String,
}

#[derive(Debug, Args)]
struct LaunchArgs {
    /// The app slug, as shown by `list`.
    app: String,
    /// Arguments passed to the app (hyphen-prefixed values are accepted
    /// directly; a `--` separator also works).
    #[arg(allow_hyphen_values = true)]
    args: Vec<String>,
    /// Print the effective plan without executing anything.
    #[arg(short = 'n', long)]
    dry_run: bool,
    /// Release the launch from the terminal: spawn, print pid and log path,
    /// and return while the process keeps running.
    #[arg(long, conflicts_with_all = ["dry_run", "json"])]
    detach: bool,
    /// Machine-readable JSON output (with --dry-run): the serialized plan.
    #[arg(long, requires = "dry_run")]
    json: bool,
}

#[derive(Debug, Args)]
struct PrefixArgs {
    #[command(subcommand)]
    command: PrefixCommand,
}

#[derive(Debug, Subcommand)]
enum PrefixCommand {
    /// Create a prefix: slug naming plus `-2` dedupe; writes the prefix file
    /// with defaults.
    Create {
        /// Display name to create, slugified automatically.
        name: String,
    },
    /// List every prefix.
    List {
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Delete a prefix and exactly its directory.
    Delete {
        /// The prefix slug, as shown by `prefix list`.
        name: String,
    },
}

#[derive(Debug, Args)]
struct DoctorArgs {
    /// Machine-readable JSON output.
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("cellar: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    let store = TreeStore::from_env()?;
    match cli.command {
        Command::Install(args) => {
            let service = InstallService::new(store.clone());
            let result =
                service.install(&args.path, &args.prefix, args.name.as_deref(), args.kind)?;
            // The flagship summary (blueprint §8 step 4): what was
            // registered, plus the next command.
            let verb = if result.was_update {
                "Updated"
            } else {
                "Registered"
            };
            println!(
                "{verb} '{}' ({}) in prefix '{}'",
                result.entry.slug,
                result.entry.kind.as_str(),
                result.entry.prefix
            );
            println!("Run it with: cellar launch {}", result.entry.slug);
            Ok(ExitCode::SUCCESS)
        }
        Command::List(args) => {
            let service = InstallService::new(store.clone());
            let entries = service.list()?;
            print!("{}", render_app_list(&entries, args.json)?);
            Ok(ExitCode::SUCCESS)
        }
        Command::Launch(args) => {
            let service = LaunchApp::new(store.clone(), ResolverSet::new(all_resolvers()));
            if args.dry_run {
                // Pre-plan phases only: resolve → check → plan, pure and
                // printable — nothing spawns (blueprint §7).
                let plan = service.plan(&args.app, &args.args)?;
                print!("{}", render_plan(&plan, args.json)?);
                return Ok(ExitCode::SUCCESS);
            }
            // Execute phase (blueprint §7): spawn the frozen plan; the
            // wait-vs-detach policy is presentation's (CLI foregrounds,
            // --detach releases the process from the terminal).
            let mode = if args.detach {
                LaunchMode::Detached
            } else {
                LaunchMode::Foreground
            };
            let process = service.spawn(&args.app, &args.args, mode)?;
            if args.detach {
                println!(
                    "Detached '{}' — pid {}, output: {}",
                    args.app,
                    process.pid(),
                    process.log_path().display()
                );
                return Ok(ExitCode::SUCCESS);
            }
            let status = process.wait()?;
            Ok(exit_code_for(status))
        }
        Command::Uninstall(args) => {
            let service = InstallService::new(store);
            service.uninstall(&args.slug)?;
            // Glossary: Uninstall — entry removal for now; Cellar never
            // deletes the app's own files.
            println!(
                "Uninstalled '{}' — entry removed; Cellar never deletes the app's own files",
                args.slug
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Prefix(args) => match args.command {
            PrefixCommand::Create { name } => {
                let service = PrefixService::new(store.clone());
                let prefix = service.create(&name)?;
                let path = store.prefix_dir(&prefix.slug);
                println!("Created prefix '{}' at {}", prefix.slug, path.display());
                Ok(ExitCode::SUCCESS)
            }
            PrefixCommand::List { json } => {
                let service = PrefixService::new(store.clone());
                let prefixes = service.list()?;
                print!("{}", render_prefix_list(&prefixes, json)?);
                Ok(ExitCode::SUCCESS)
            }
            PrefixCommand::Delete { name } => {
                let service = PrefixService::new(store.clone());
                service.delete(&name)?;
                println!("Deleted prefix '{name}'");
                Ok(ExitCode::SUCCESS)
            }
        },
        Command::Doctor(args) => {
            let service = DoctorService::new(store);
            let health = service.tree_health()?;
            print!("{}", render_tree_health(&health, args.json)?);
            if health.is_healthy() {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::from(1))
            }
        }
    }
}

/// The game's exit status → our exit code (blueprint §7 Runtime family, ADR
/// 0004): a real exit code propagates raw — including 1, whose collision
/// with Cellar's operation-error code is documented, not mapped. A
/// signal-terminated process has no code to propagate: the signal is
/// reported to stderr and Cellar exits 1.
fn exit_code_for(status: ExitStatus) -> ExitCode {
    if let Some(code) = status.code() {
        return ExitCode::from(raw_exit_code(code));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            eprintln!("the game was terminated by a signal ({signal})");
        }
    }
    ExitCode::FAILURE
}

/// The raw exit code as an `ExitCode`-compatible byte. Unix exit codes are
/// 0–255 by POSIX; anything else (conceivable only off Unix) degrades to 1
/// rather than silently truncating a code the terminal never reported.
fn raw_exit_code(code: i32) -> u8 {
    u8::try_from(code).unwrap_or(1)
}

/// A column table: header row, blank line, then padded, two-space-separated
/// rows. Shared by `list` and `prefix list` — one table shape for both.
fn render_table<const N: usize>(headers: [&str; N], mut rows: Vec<[String; N]>) -> String {
    rows.insert(0, headers.map(str::to_owned));
    let widths: Vec<usize> = (0..N)
        .map(|col| rows.iter().map(|row| row[col].len()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        let cells: Vec<String> = (0..N)
            .map(|col| format!("{:width$}", row[col], width = widths[col]))
            .collect();
        out.push_str(&cells.join("  "));
        out.push('\n');
        if i == 0 {
            out.push('\n');
        }
    }
    out
}

/// The human table (`list`): slug plus the prefix defaults a user can
/// hand-edit — runner, graphics, Windows version.
fn render_prefix_list(prefixes: &[Prefix], json: bool) -> anyhow::Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(prefixes)?);
    }
    let rows: Vec<[String; 4]> = prefixes
        .iter()
        .map(|p| {
            let graphics = p.defaults.graphics.as_deref().unwrap_or("–");
            let windows = p.defaults.windows_version.as_deref().unwrap_or("–");
            [
                p.slug.clone(),
                runner_label(p.defaults.runner.as_ref()),
                graphics.to_owned(),
                windows.to_owned(),
            ]
        })
        .collect();
    Ok(render_table(
        ["Slug", "Runner", "Graphics", "Windows"],
        rows,
    ))
}

/// The runner column: family plus pin when configured.
fn runner_label(runner: Option<&cellar_core::RunnerSpec>) -> String {
    let Some(spec) = runner else {
        return "–".to_owned();
    };
    let family = spec.family.as_str();
    match &spec.configured {
        None => family.to_owned(),
        Some(cellar_core::ConfiguredRunner::Path(path)) => format!("{family} {}", path.display()),
        Some(cellar_core::ConfiguredRunner::Version(version)) => format!("{family} {version}"),
    }
}

/// Label of a pinned runner reference (provider plus its resolved state).
fn runner_ref_label(runner: &RunnerRef) -> String {
    match &runner.install {
        cellar_core::RunnerInstall::Managed { version, .. } => {
            format!("{} {version}", runner.provider_id)
        }
        cellar_core::RunnerInstall::Discovered { path, version } => match version {
            Some(version) => format!("{} {version}", runner.provider_id),
            None => format!("{} {}", runner.provider_id, path.display()),
        },
    }
}

/// The runner column of an entry: pinned ref → app override → bound
/// prefix's default → the defaults-floor preset (Game → Proton, Tool →
/// wine). The prefix default rung joins with the launch slice (#28); the
/// effective runner of a real launch is what `launch --dry-run` prints.
fn runner_for(entry: &AppEntry, prefix_runner: Option<&cellar_core::RunnerSpec>) -> String {
    if let Some(runner) = &entry.runner {
        return runner_ref_label(runner);
    }
    if let Some(spec) = &entry.overrides.runner {
        return runner_label(Some(spec));
    }
    if let Some(spec) = prefix_runner {
        return runner_label(Some(spec));
    }
    entry.kind.default_family().as_str().to_owned()
}

/// The human table (`list`): slug, kind, prefix, runner, status
/// (blueprint §8) — or the machine JSON (entry fields plus status).
fn render_app_list(entries: &[ListedEntry], json: bool) -> anyhow::Result<String> {
    if json {
        let rows: Vec<JsonApp> = entries
            .iter()
            .map(|listed| JsonApp {
                entry: &listed.entry,
                status: listed.status.as_str(),
            })
            .collect();
        return Ok(serde_json::to_string_pretty(&rows)?);
    }
    let rows: Vec<[String; 5]> = entries
        .iter()
        .map(|listed| {
            [
                listed.entry.slug.clone(),
                listed.entry.kind.as_str().to_owned(),
                listed.entry.prefix.clone(),
                runner_for(&listed.entry, listed.prefix_runner.as_ref()),
                listed.status.as_str().to_owned(),
            ]
        })
        .collect();
    Ok(render_table(
        ["Slug", "Kind", "Prefix", "Runner", "Status"],
        rows,
    ))
}

/// The JSON row shape of `list --json`: the entry itself (all fields, as
/// serialized) plus its status label.
#[derive(serde::Serialize)]
struct JsonApp<'a> {
    #[serde(flatten)]
    entry: &'a AppEntry,
    status: &'static str,
}

/// The dry-run render: argv, env, and the wrapper chain — the effective
/// plan's three surfaces (blueprint §7) — or the machine JSON: the plan
/// itself, serialized, a reproducible artifact for bug reports.
fn render_plan(plan: &LaunchPlan, json: bool) -> anyhow::Result<String> {
    use std::fmt::Write;

    if json {
        return Ok(serde_json::to_string_pretty(plan)?);
    }
    let mut out = String::new();
    writeln!(out, "  argv: {}", plan.argv.join(" "))?;
    if plan.env.is_empty() {
        writeln!(out, "  env:  (none)")?;
    } else {
        writeln!(out, "  env:")?;
        for (key, value) in &plan.env {
            writeln!(out, "    {key}={value}")?;
        }
    }
    if plan.wrappers.is_empty() {
        writeln!(out, "  wrappers: none")?;
    } else {
        let chain = plan
            .wrappers
            .iter()
            .map(|layer| layer.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(out, "  wrappers: {chain}")?;
    }
    if let Some(cwd) = &plan.cwd {
        writeln!(out, "  cwd:  {}", cwd.display())?;
    }
    Ok(out)
}

/// Composition-root glue (blueprint §4): the registry's heterogeneous
/// resolver collection wrapped as a single `RunnerResolver`, tried in
/// registry order. The error of the provider that services the spec's
/// family wins over another family's "not me" — the message then names
/// what a `SuggestInstall` must find (blueprint §7). `Box<dyn _>` lives
/// here, at the composition root, nowhere below presentation.
#[derive(Debug)]
struct ResolverSet {
    resolvers: Vec<Box<dyn RunnerResolver>>,
}

impl ResolverSet {
    fn new(resolvers: Vec<Box<dyn RunnerResolver>>) -> Self {
        Self { resolvers }
    }
}

impl __sealed::Sealed for ResolverSet {}

impl RunnerResolver for ResolverSet {
    fn id(&self) -> &'static str {
        "registry"
    }

    fn resolve(&self, spec: &RunnerSpec) -> Result<ResolvedRunner, ResolveError> {
        let mut last_error = ResolveError::Unresolvable {
            family: spec.family,
        };
        let mut serviced = None;
        for resolver in &self.resolvers {
            match resolver.resolve(spec) {
                Ok(resolved) => return Ok(resolved),
                Err(err) => {
                    last_error = err;
                    if last_error.family() == spec.family {
                        serviced = Some(last_error.clone());
                    }
                }
            }
        }
        Err(serviced.unwrap_or(last_error))
    }
}

/// The doctor's tree section: one pass/fail line per check, every failure
/// with a fix hint (blueprint §8).
fn render_tree_health(health: &TreeHealth, json: bool) -> anyhow::Result<String> {
    use std::fmt::Write;

    if json {
        return Ok(serde_json::to_string_pretty(health)?);
    }
    let mut out = String::from("Tree checks:\n");
    if !health.tree_exists {
        writeln!(
            out,
            "  ✗ tree missing at {} — doctor only reports; the next prefix \
             command creates it",
            health.root.display()
        )?;
        return Ok(out);
    }
    writeln!(out, "  ✓ root: {}", health.root.display())?;
    if health.missing_dirs.is_empty() {
        writeln!(out, "  ✓ directories: prefixes, apps, runtime, cache")?;
    }
    for dir in &health.missing_dirs {
        writeln!(
            out,
            "  ✗ missing directory: {} — Cellar recreates it on the next command",
            dir.display()
        )?;
    }
    if health.missing_files.is_empty()
        && health.invalid_files.is_empty()
        && health.missing_exes.is_empty()
    {
        writeln!(
            out,
            "  ✓ files and entries: settings.toml, every entry, and every registered exe parse"
        )?;
    }
    for file in &health.missing_files {
        writeln!(
            out,
            "  ✗ missing file: {} — Cellar recreates defaults on the next command",
            file.display()
        )?;
    }
    for file in &health.invalid_files {
        writeln!(
            out,
            "  ✗ invalid file: {} — not read; repair or delete it (Cellar never \
             overwrites hand edits)",
            file.display()
        )?;
    }
    for slug in &health.missing_exes {
        writeln!(
            out,
            "  ✗ missing exe for entry '{slug}' — the registered exe is gone; \
             re-register it with `cellar install` or uninstall the entry"
        )?;
    }
    for dir in &health.orphan_prefix_dirs {
        writeln!(
            out,
            "  ✗ orphan prefix directory: {} — no prefix.toml inside; delete it",
            dir.display()
        )?;
    }
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use cellar_app::{EntryStatus, InstallService, ListedEntry, PrefixService};
    use cellar_core::ports::Storage as _;
    use cellar_core::{
        AppEntry, AppKind, Overrides, PrefixDefaults, RunnerFamily, RunnerInstall, RunnerRef,
    };

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn parses_prefix_create() {
        let cli = Cli::try_parse_from(["cellar", "prefix", "create", "my-games"])
            .unwrap_or_else(|e| panic!("parse: {e}"));
        let Command::Prefix(PrefixArgs {
            command: PrefixCommand::Create { name },
        }) = cli.command
        else {
            panic!("unexpected command");
        };
        assert_eq!(name, "my-games");
    }

    #[test]
    fn parses_prefix_list_with_json() {
        let cli = Cli::try_parse_from(["cellar", "prefix", "list", "--json"])
            .unwrap_or_else(|e| panic!("parse: {e}"));
        let Command::Prefix(PrefixArgs {
            command: PrefixCommand::List { json },
        }) = cli.command
        else {
            panic!("unexpected command");
        };
        assert!(json);
    }

    #[test]
    fn parses_delete_and_doctor() {
        let cli = Cli::try_parse_from(["cellar", "prefix", "delete", "games"])
            .unwrap_or_else(|e| panic!("parse: {e}"));
        assert!(matches!(
            cli.command,
            Command::Prefix(PrefixArgs {
                command: PrefixCommand::Delete { .. }
            })
        ));
        let cli =
            Cli::try_parse_from(["cellar", "doctor"]).unwrap_or_else(|e| panic!("parse: {e}"));
        assert!(matches!(cli.command, Command::Doctor(_)));
    }

    #[test]
    fn usage_errors_exit_with_code_two() {
        assert!(Cli::try_parse_from(["cellar", "prefix", "bogus"]).is_err());
        assert!(Cli::try_parse_from(["cellar"]).is_err());
    }

    #[test]
    fn prefix_list_renders_human_table_and_json() -> anyhow::Result<()> {
        let prefix = Prefix {
            slug: "my-games".to_owned(),
            defaults: PrefixDefaults::default(),
        };
        let human = render_prefix_list(std::slice::from_ref(&prefix), false)?;
        assert!(human.contains("Slug"), "header missing:\n{human}");
        assert!(human.contains("my-games"), "row missing:\n{human}");
        let json = render_prefix_list(&[prefix], true)?;
        assert!(
            json.contains("\"slug\": \"my-games\""),
            "json missing:\n{json}"
        );
        Ok(())
    }

    #[test]
    fn doctor_renders_pass_and_fail_sections() -> anyhow::Result<()> {
        let healthy = TreeHealth {
            root: PathBuf::from("/tmp/cellar"),
            tree_exists: true,
            missing_dirs: Vec::new(),
            missing_files: Vec::new(),
            invalid_files: Vec::new(),
            orphan_prefix_dirs: Vec::new(),
            missing_exes: Vec::new(),
            schema_version: 1,
        };
        let out = render_tree_health(&healthy, false)?;
        assert!(out.contains("✓"), "healthy tree should pass:\n{out}");
        let mut broken = healthy.clone();
        broken
            .invalid_files
            .push(PathBuf::from("prefixes/x/prefix.toml"));
        let out = render_tree_health(&broken, false)?;
        assert!(out.contains('✗'), "broken tree should fail:\n{out}");
        assert!(out.contains("never overwrites"), "fix hint missing:\n{out}");
        let mut gone = healthy;
        gone.missing_exes.push("balatro".to_owned());
        let out = render_tree_health(&gone, false)?;
        assert!(out.contains('✗'), "missing exe should fail:\n{out}");
        assert!(
            out.contains("missing exe for entry 'balatro'"),
            "flag missing:\n{out}"
        );
        assert!(out.contains("re-register it"), "fix hint missing:\n{out}");
        Ok(())
    }

    #[test]
    fn parses_install_with_defaults_and_flags() {
        let cli = Cli::try_parse_from(["cellar", "install", "/games/balatro.exe"])
            .unwrap_or_else(|e| panic!("parse: {e}"));
        let Command::Install(args) = cli.command else {
            panic!("unexpected command");
        };
        assert_eq!(args.path, PathBuf::from("/games/balatro.exe"));
        assert_eq!(args.prefix, "default", "the default prefix is `default`");
        assert!(
            args.name.is_none(),
            "the name defaults to the exe file name"
        );
        assert_eq!(args.kind, AppKind::Game, "the default kind is `game`");
        let cli = Cli::try_parse_from([
            "cellar",
            "install",
            "/games/balatro.exe",
            "--prefix",
            "games",
            "--name",
            "Balatro",
            "--kind",
            "tool",
        ])
        .unwrap_or_else(|e| panic!("parse: {e}"));
        let Command::Install(args) = cli.command else {
            panic!("unexpected command");
        };
        assert_eq!(args.prefix, "games");
        assert_eq!(args.name.as_deref(), Some("Balatro"));
        assert_eq!(args.kind, AppKind::Tool);
    }

    #[test]
    fn unknown_kind_is_a_usage_error() {
        assert!(Cli::try_parse_from(["cellar", "install", "x.exe", "--kind", "app"]).is_err());
    }

    #[test]
    fn parses_launch_with_dry_run_and_app_args() {
        let cli = Cli::try_parse_from(["cellar", "launch", "balatro", "-n"])
            .unwrap_or_else(|e| panic!("parse: {e}"));
        let Command::Launch(LaunchArgs {
            app,
            args,
            dry_run,
            detach,
            json,
        }) = cli.command
        else {
            panic!("unexpected command");
        };
        assert_eq!(app, "balatro");
        assert!(dry_run, "-n is the dry-run shorthand");
        assert!(args.is_empty());
        assert!(!detach);
        assert!(!json);
        let cli = Cli::try_parse_from([
            "cellar",
            "launch",
            "balatro",
            "--dry-run",
            "--json",
            "--",
            "--fullscreen",
        ])
        .unwrap_or_else(|e| panic!("parse: {e}"));
        let Command::Launch(LaunchArgs {
            app,
            args,
            dry_run,
            detach,
            json,
        }) = cli.command
        else {
            panic!("unexpected command");
        };
        assert_eq!(app, "balatro");
        assert!(dry_run);
        assert!(!detach);
        assert!(json);
        assert_eq!(args, ["--fullscreen"], "app args come after `--`");
    }

    #[test]
    fn parses_launch_detach() {
        let cli = Cli::try_parse_from(["cellar", "launch", "balatro", "--detach"])
            .unwrap_or_else(|e| panic!("parse: {e}"));
        let Command::Launch(LaunchArgs { detach, .. }) = cli.command else {
            panic!("unexpected command");
        };
        assert!(detach);
        // Detaching is a spawn policy — it cannot combine with the
        // spawn-free preview or its JSON render.
        assert!(
            Cli::try_parse_from(["cellar", "launch", "balatro", "--detach", "--dry-run"]).is_err(),
            "--detach conflicts with --dry-run"
        );
        assert!(
            Cli::try_parse_from(["cellar", "launch", "balatro", "--detach", "--json"]).is_err(),
            "--json requires --dry-run, which --detach conflicts with"
        );
    }

    #[test]
    #[cfg(unix)]
    fn launch_exit_codes_propagate_raw() {
        use std::os::unix::process::ExitStatusExt;

        // A real wait-status encodes the exit code in the high byte.
        assert_eq!(
            raw_exit_code(ExitStatus::from_raw(7 << 8).code().unwrap()),
            7
        );
        assert_eq!(raw_exit_code(ExitStatus::from_raw(0).code().unwrap()), 0);
        // Signal deaths have no code — the raw path degrades to 1 (the
        // signal itself is reported to stderr by `exit_code_for`).
        assert_eq!(
            raw_exit_code(ExitStatus::from_raw(0x02).code().unwrap_or(1)),
            1
        );
    }

    #[test]
    fn launch_json_requires_dry_run() {
        assert!(
            Cli::try_parse_from(["cellar", "launch", "balatro", "--json"]).is_err(),
            "--json without --dry-run is a usage error"
        );
        assert!(
            Cli::try_parse_from(["cellar", "launch", "balatro"]).is_ok(),
            "a plain launch parses and executes for real (#29)"
        );
    }

    #[test]
    fn parses_list_and_uninstall() {
        let cli = Cli::try_parse_from(["cellar", "list", "--json"])
            .unwrap_or_else(|e| panic!("parse: {e}"));
        let Command::List(ListArgs { json }) = cli.command else {
            panic!("unexpected command");
        };
        assert!(json);
        let cli = Cli::try_parse_from(["cellar", "uninstall", "balatro"])
            .unwrap_or_else(|e| panic!("parse: {e}"));
        let Command::Uninstall(UninstallArgs { slug }) = cli.command else {
            panic!("unexpected command");
        };
        assert_eq!(slug, "balatro");
    }

    /// A listed entry for render tests: game in `default`, exe present.
    fn listed_entry(slug: &str, kind: AppKind, status: EntryStatus) -> ListedEntry {
        ListedEntry {
            entry: AppEntry {
                slug: slug.to_owned(),
                exe: PathBuf::from(format!("/games/{slug}.exe")),
                kind,
                prefix: "default".to_owned(),
                overrides: Overrides::default(),
                runner: None,
                source_installer: None,
                installed_at: None,
            },
            status,
            prefix_runner: None,
        }
    }

    #[test]
    fn app_list_renders_human_table_and_json() -> anyhow::Result<()> {
        let listed = listed_entry("balatro", AppKind::Game, EntryStatus::ExeMissing);
        let human = render_app_list(std::slice::from_ref(&listed), false)?;
        for header in ["Slug", "Kind", "Prefix", "Runner", "Status"] {
            assert!(human.contains(header), "header {header} missing:\n{human}");
        }
        assert!(human.contains("balatro"), "row missing:\n{human}");
        assert!(human.contains("game"), "kind label missing:\n{human}");
        assert!(
            human.contains("proton"),
            "defaults-floor preset missing:\n{human}"
        );
        assert!(human.contains("missing-exe"), "status missing:\n{human}");
        let json = render_app_list(&[listed], true)?;
        assert!(
            json.contains("\"slug\": \"balatro\""),
            "json missing:\n{json}"
        );
        assert!(
            json.contains("\"kind\": \"game\""),
            "kind vocabulary missing:\n{json}"
        );
        assert!(
            json.contains("\"status\": \"missing-exe\""),
            "status missing:\n{json}"
        );
        Ok(())
    }

    #[test]
    fn runner_column_honors_override_then_prefix_default_then_preset() {
        let entry = listed_entry("balatro", AppKind::Game, EntryStatus::Ok).entry;
        assert_eq!(
            runner_for(&entry, None),
            "proton",
            "the defaults-floor preset for games"
        );
        let mut tooled = entry.clone();
        tooled.kind = AppKind::Tool;
        assert_eq!(
            runner_for(&tooled, None),
            "wine",
            "the defaults-floor preset for tools"
        );
        let prefix_default = Some(cellar_core::RunnerSpec::with_configured(
            RunnerFamily::Wine,
            cellar_core::ConfiguredRunner::Path(PathBuf::from("/opt/wine/bin/wine")),
        ));
        assert_eq!(
            runner_for(&tooled, prefix_default.as_ref()),
            "wine /opt/wine/bin/wine",
            "the bound prefix's default beats the kind preset"
        );
        let mut overridden = entry.clone();
        overridden.overrides.runner = Some(cellar_core::RunnerSpec::new(RunnerFamily::Wine));
        assert_eq!(
            runner_for(&overridden, None),
            "wine",
            "an app override beats the preset"
        );
        let mut pinned = entry;
        pinned.runner = Some(RunnerRef {
            provider_id: "proton".to_owned(),
            family: RunnerFamily::Proton,
            install: RunnerInstall::Managed {
                version: "9.0-4".to_owned(),
                path: PathBuf::from("/opt/proton"),
            },
        });
        assert_eq!(
            runner_for(&pinned, None),
            "proton 9.0-4",
            "a pinned ref wins"
        );
    }

    #[test]
    fn app_registry_lifecycle_end_to_end() -> anyhow::Result<()> {
        use cellar_app::DoctorService;

        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("cellar-cli-e2e-apps-{}-{seq}", std::process::id()));
        let store = TreeStore::new(root.clone());
        let service = InstallService::new(store.clone());
        let exe = root.join("drive_c/My Game.exe");
        std::fs::create_dir_all(exe.parent().unwrap_or(Path::new(".")))
            .unwrap_or_else(|e| panic!("mkdir: {e}"));
        std::fs::write(&exe, "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        let first = service.install(&exe, "default", None, AppKind::Game)?;
        assert!(!first.was_update);
        assert_eq!(first.entry.slug, "my-game");
        assert_eq!(
            first.entry.exe,
            std::fs::canonicalize(&exe).unwrap_or_else(|e| panic!("canonicalize: {e}")),
            "identity is the canonical exe path"
        );
        let second = service.install(&exe, "default", None, AppKind::Tool)?;
        assert!(second.was_update, "re-install updates the same entry");
        assert_eq!(second.entry.slug, "my-game");
        assert_eq!(service.list()?.len(), 1);
        let listed = service.list()?;
        assert_eq!(listed[0].status.as_str(), "ok");
        std::fs::remove_file(&exe).unwrap_or_else(|e| panic!("remove: {e}"));
        let listed = service.list()?;
        assert_eq!(listed[0].status.as_str(), "missing-exe");
        let health = DoctorService::new(store.clone()).tree_health()?;
        assert_eq!(health.missing_exes, ["my-game"]);
        service.uninstall("my-game")?;
        assert!(service.list()?.is_empty());
        Ok(())
    }

    #[test]
    fn prefix_lifecycle_end_to_end() -> anyhow::Result<()> {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("cellar-cli-e2e-{}-{seq}", std::process::id()));
        let store = TreeStore::new(root);
        let service = PrefixService::new(store.clone());
        let first = service.create("My Games")?;
        assert_eq!(first.slug, "my-games");
        let second = service.create("My Games")?;
        assert_eq!(second.slug, "my-games-2");
        assert_eq!(service.list()?.len(), 2);
        service.delete("my-games")?;
        assert_eq!(service.list()?.len(), 1);
        Ok(())
    }

    #[test]
    fn render_plan_shows_argv_env_and_wrappers() -> anyhow::Result<()> {
        let plan = LaunchPlan {
            argv: vec!["/usr/bin/wine".to_owned(), "/games/balatro.exe".to_owned()],
            env: BTreeMap::from([("WINEPREFIX".to_owned(), "/root/prefixes/default".to_owned())]),
            cwd: None,
            wrappers: vec![],
        };
        let human = render_plan(&plan, false)?;
        assert!(
            human.contains("argv: /usr/bin/wine /games/balatro.exe"),
            "argv line missing:\n{human}"
        );
        assert!(
            human.contains("WINEPREFIX=/root/prefixes/default"),
            "env missing:\n{human}"
        );
        assert!(human.contains("wrappers: none"), "chain missing:\n{human}");
        let mut wrapped = plan.clone();
        wrapped.wrappers = vec![cellar_core::Layer::Container];
        let human = render_plan(&wrapped, false)?;
        assert!(
            human.contains("wrappers: container"),
            "chain missing:\n{human}"
        );
        let json = render_plan(&plan, true)?;
        assert!(
            json.contains("\"argv\""),
            "json missing the plan fields:\n{json}"
        );
        Ok(())
    }

    #[test]
    fn resolver_set_keeps_the_serviced_familys_error() {
        // Providers answer "not me" with their own family; the set must
        // prefer the error of the provider that services the spec's family
        // — otherwise a wine spec would report proton's exhaustion.
        let set = ResolverSet::new(vec![
            Box::new(StubResolver {
                family: RunnerFamily::Proton,
                outcome: Err(ResolveError::Unresolvable {
                    family: RunnerFamily::Proton,
                }),
            }),
            Box::new(StubResolver {
                family: RunnerFamily::Wine,
                outcome: Err(ResolveError::Unresolvable {
                    family: RunnerFamily::Wine,
                }),
            }),
        ]);
        let err = set
            .resolve(&RunnerSpec::new(RunnerFamily::Wine))
            .expect_err("all stubs fail");
        assert_eq!(
            err,
            ResolveError::Unresolvable {
                family: RunnerFamily::Wine
            },
            "the serviced family's error wins"
        );
        assert!(
            err.to_string().contains("wine"),
            "the message names what to install: {err}"
        );
    }

    #[test]
    fn resolver_set_first_success_wins() {
        let set = ResolverSet::new(vec![
            Box::new(StubResolver {
                family: RunnerFamily::Proton,
                outcome: Err(ResolveError::Unresolvable {
                    family: RunnerFamily::Proton,
                }),
            }),
            Box::new(StubResolver {
                family: RunnerFamily::Wine,
                outcome: Ok(ResolvedRunner {
                    mode: cellar_core::ProviderMode::DiscoverOnly,
                    reference: RunnerRef {
                        provider_id: "wine".to_owned(),
                        family: RunnerFamily::Wine,
                        install: RunnerInstall::Discovered {
                            path: PathBuf::from("/usr/bin/wine"),
                            version: None,
                        },
                    },
                }),
            }),
        ]);
        let resolved = set
            .resolve(&RunnerSpec::new(RunnerFamily::Wine))
            .unwrap_or_else(|e| panic!("resolve: {e}"));
        assert_eq!(resolved.mode, cellar_core::ProviderMode::DiscoverOnly);
    }

    /// A canned resolver double for composition-root tests: answers its
    /// canned outcome regardless of the spec.
    #[derive(Debug)]
    struct StubResolver {
        family: RunnerFamily,
        outcome: Result<ResolvedRunner, ResolveError>,
    }

    impl cellar_core::ports::__sealed::Sealed for StubResolver {}

    impl RunnerResolver for StubResolver {
        fn id(&self) -> &'static str {
            match self.family {
                RunnerFamily::Proton => "proton",
                RunnerFamily::Wine => "wine",
                RunnerFamily::Umu => "umu",
            }
        }

        fn resolve(&self, _spec: &RunnerSpec) -> Result<ResolvedRunner, ResolveError> {
            self.outcome.clone()
        }
    }

    /// Write an executable stub script the way every spawn test does: create →
    /// write → `sync_all` → drop, so the script leaves the write-open state
    /// (the exec `ETXTBSY` window) before any spawn — one pattern, no
    /// drifted copies (mirrored in `cellar-launch` and `cellar-app`).
    #[cfg(unix)]
    fn write_stub_script(path: &Path, body: &str) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let mut file = std::fs::File::create(path)?;
        file.write_all(format!("#!/bin/sh\n{body}\n").as_bytes())?;
        file.sync_all()?;
        drop(file);
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)
    }

    #[test]
    #[cfg(unix)]
    fn launch_executes_the_plan_end_to_end_with_a_stub_wine() -> anyhow::Result<()> {
        use cellar_core::ConfiguredRunner;

        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-execute-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(root.clone());
        std::fs::create_dir_all(&root)?;
        // The configured stub runner wins resolution — no real wine needed.
        // It echoes the plan it received (argv, the WINEPREFIX contract,
        // both streams) and exits 7: the raw-code and per-launch-log
        // acceptance criteria, in one binary.
        let wine = root.join("stub-wine");
        write_stub_script(
            &wine,
            "echo \"argv=$*\"\necho \"wineprefix=$WINEPREFIX\"\necho \"out-line\"\necho \"err-line\" >&2\nexit 7\n",
        )?;
        let exe = root.join("drive_c/tool.exe");
        std::fs::create_dir_all(exe.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(&exe, "MZ")?;
        let registered = InstallService::new(store.clone()).install(
            &exe,
            "default",
            Some("My Tool"),
            AppKind::Tool,
        )?;
        let mut prefix = store.load_prefix("default")?;
        prefix.defaults.runner = Some(RunnerSpec::with_configured(
            RunnerFamily::Wine,
            ConfiguredRunner::Path(wine.clone()),
        ));
        store.save_prefix(&prefix)?;
        let service = LaunchApp::new(store.clone(), ResolverSet::new(all_resolvers()));
        // Foreground: spawn the frozen plan, await it, propagate the raw
        // exit code — 7, not a Cellar error.
        let process = service.spawn(
            &registered.entry.slug,
            &["--fullscreen".to_owned()],
            LaunchMode::Foreground,
        )?;
        let log_path = process.log_path().to_path_buf();
        let status = process.wait()?;
        assert_eq!(status.code(), Some(7), "the exit code propagates raw");
        assert!(
            log_path.starts_with(store.launch_logs_dir()),
            "output lands under the disposable cache: {}",
            log_path.display()
        );
        let name = log_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        assert!(
            name.starts_with("my-tool-"),
            "per-launch log named <slug>-<timestamp>.log: {name}"
        );
        assert_eq!(
            log_path.extension().and_then(|ext| ext.to_str()),
            Some("log"),
            "the log ends in .log: {name}"
        );
        let text = std::fs::read_to_string(log_path)?;
        assert!(
            text.contains("--fullscreen"),
            "the plan's argv ran exactly:\n{text}"
        );
        assert!(
            text.contains("wineprefix=") && text.contains("prefixes/default"),
            "the plan's env contract (WINEPREFIX) applied:\n{text}"
        );
        assert!(text.contains("out-line"), "stdout missing:\n{text}");
        assert!(text.contains("err-line"), "stderr missing:\n{text}");
        // Detached: the handle returns with the process still running, and
        // its output still lands in its own per-launch log.
        let sleeper = root.join("stub-sleeper");
        write_stub_script(&sleeper, "echo sleeping\nexit 0\n")?;
        let mut prefix = store.load_prefix("default")?;
        prefix.defaults.runner = Some(RunnerSpec::with_configured(
            RunnerFamily::Wine,
            ConfiguredRunner::Path(sleeper.clone()),
        ));
        store.save_prefix(&prefix)?;
        let service = LaunchApp::new(store, ResolverSet::new(all_resolvers()));
        let process = service.spawn(&registered.entry.slug, &[], LaunchMode::Detached)?;
        let log_path = process.log_path().to_path_buf();
        // The Linux liveness probe: the pid exists while the launch runs —
        // no libc/unsafe needed.
        #[cfg(target_os = "linux")]
        assert!(
            std::fs::metadata(format!("/proc/{}", process.pid())).is_ok(),
            "--detach must return with the process still running (pid {})",
            process.pid()
        );
        let status = process.wait()?;
        assert_eq!(status.code(), Some(0), "the detached launch ran to the end");
        let text = std::fs::read_to_string(&log_path)?;
        assert!(
            text.contains("sleeping"),
            "detached output missing:\n{text}"
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn launch_dry_run_end_to_end_with_a_stub_wine() -> anyhow::Result<()> {
        use cellar_core::ConfiguredRunner;

        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-launch-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(root.clone());
        std::fs::create_dir_all(&root)?;
        // A stub wine binary: the configured path wins resolution — no PATH
        // games, and the dry-run stays spawn-free.
        let wine = root.join("stub-wine");
        write_stub_script(&wine, "exit 0\n")?;
        // Register a tool app (kind floor: wine) whose prefix defaults to
        // the configured stub — hand-edited, like a user would.
        let exe = root.join("drive_c/tool.exe");
        std::fs::create_dir_all(exe.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(&exe, "MZ")?;
        let registered = InstallService::new(store.clone()).install(
            &exe,
            "default",
            Some("My Tool"),
            AppKind::Tool,
        )?;
        let mut prefix = store.load_prefix("default")?;
        prefix.defaults.runner = Some(RunnerSpec::with_configured(
            RunnerFamily::Wine,
            ConfiguredRunner::Path(wine),
        ));
        store.save_prefix(&prefix)?;
        // Plan it: resolve → check → plan, pure and printable.
        let app = LaunchApp::new(store.clone(), ResolverSet::new(all_resolvers()));
        let plan = app.plan(&registered.entry.slug, &["--fullscreen".to_owned()])?;
        let canonical_exe = std::fs::canonicalize(&exe)?;
        assert_eq!(
            plan.argv,
            [
                root.join("stub-wine").to_string_lossy().into_owned(),
                canonical_exe.to_string_lossy().into_owned(),
                "--fullscreen".to_owned(),
            ],
            "argv = configured runner, canonical exe, args"
        );
        let expected_prefix = store.prefix_dir(&registered.entry.prefix);
        assert_eq!(
            plan.env.get("WINEPREFIX").map(String::as_str),
            Some(expected_prefix.to_string_lossy().as_ref()),
            "the plan pins the bound prefix"
        );
        assert!(plan.wrappers.is_empty());
        // The human render is the printable dry-run surface.
        let human = render_plan(&plan, false)?;
        assert!(human.contains("argv:"), "render missing:\n{human}");
        // The JSON render is the machine surface.
        let json = render_plan(&plan, true)?;
        assert!(json.contains("\"argv\""), "json missing:\n{json}");
        // Pre-flight dispositions against the real tree: a deleted exe
        // re-registers; an unresolvable runner suggests install.
        std::fs::remove_file(&exe)?;
        let err = app.plan(&registered.entry.slug, &[]).expect_err("exe gone");
        assert!(
            err.to_string().contains("re-register"),
            "disposition missing: {err}"
        );
        // A game in a runnerless prefix hits the kind floor (Proton), which
        // nothing resolves before the managed pipeline — SuggestInstall.
        let second_store = store.clone();
        let games = PrefixService::new(store.clone()).create("games")?;
        let game_root = root.join("game-exe");
        std::fs::create_dir_all(&game_root)?;
        let game_exe = game_root.join("game.exe");
        std::fs::write(&game_exe, "MZ")?;
        let game = InstallService::new(store.clone()).install(
            &game_exe,
            &games.slug,
            None,
            AppKind::Game,
        )?;
        let app = LaunchApp::new(second_store, ResolverSet::new(all_resolvers()));
        let err = app.plan(&game.entry.slug, &[]).expect_err("no proton yet");
        assert!(
            err.to_string().contains("proton"),
            "the SuggestInstall message names the family: {err}"
        );
        Ok(())
    }
}
