//! The Cellar CLI — presentation, composition root (blueprint §8, ADR 0004).
//!
//! This binary is the *only* place concrete infra is instantiated: it builds
//! the tree adapter ([`TreeStore`]) and injects it into the application
//! services, which are generic over the `core` ports. Symmetric with the
//! future `cellar-gui` leaf; flipping primary is `default-members`, zero
//! edits below presentation.
//!
//! Surface for this slice (#27): `cellar install <exe>` (the standalone
//! branch — registered without executing), `cellar list` (apps table),
//! `cellar uninstall <app>` — on top of #26's `cellar prefix …` and
//! `cellar doctor`. Exit codes (ADR 0004): 0 success, 1 operation error or
//! doctor problems, 2 usage (clap).

use clap::{Args, Parser, Subcommand};

use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

use cellar_app::{DoctorService, InstallService, ListedEntry, PrefixService};
use cellar_core::{AppEntry, AppKind, Prefix, RunnerRef, TreeHealth};
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

/// The runner column of an entry: pinned ref → app override → the
/// defaults-floor preset (Game → Proton, Tool → wine). The prefix default
/// joins the chain with the launch slice (#28), which resolves the effective
/// runner.
fn runner_for(entry: &AppEntry) -> String {
    if let Some(runner) = &entry.runner {
        return runner_ref_label(runner);
    }
    if let Some(spec) = &entry.overrides.runner {
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
                runner_for(&listed.entry),
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

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use cellar_app::{EntryStatus, InstallService, ListedEntry, PrefixService};
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
    fn runner_column_honors_override_then_preset() {
        let entry = listed_entry("balatro", AppKind::Game, EntryStatus::Ok).entry;
        assert_eq!(
            runner_for(&entry),
            "proton",
            "the defaults-floor preset for games"
        );
        let mut tooled = entry.clone();
        tooled.kind = AppKind::Tool;
        assert_eq!(
            runner_for(&tooled),
            "wine",
            "the defaults-floor preset for tools"
        );
        let mut overridden = entry.clone();
        overridden.overrides.runner = Some(cellar_core::RunnerSpec::new(RunnerFamily::Wine));
        assert_eq!(
            runner_for(&overridden),
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
        assert_eq!(runner_for(&pinned), "proton 9.0-4", "a pinned ref wins");
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
}
