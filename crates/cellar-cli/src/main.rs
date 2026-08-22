//! The Cellar CLI — presentation, composition root (blueprint §8, ADR 0004).
//!
//! This binary is the *only* place concrete infra is instantiated: it builds
//! the tree adapter ([`TreeStore`]) and injects it into the application
//! services, which are generic over the `core` ports. Symmetric with the
//! future `cellar-gui` leaf; flipping primary is `default-members`, zero
//! edits below presentation.
//!
//! Surface for this slice (#26): `cellar prefix create|list|delete` and
//! `cellar doctor` — the tree-health check. Exit codes (ADR 0004): 0
//! success, 1 operation error or doctor problems, 2 usage (clap).

use clap::{Args, Parser, Subcommand};

use std::process::ExitCode;

use cellar_app::{DoctorService, PrefixService};
use cellar_core::{Prefix, TreeHealth};
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
    /// Manage Cellar prefixes (blueprint §8: lifecycle objects get noun
    /// groups).
    Prefix(PrefixArgs),
    /// Sectioned capability checks with fix hints; exits 1 on any problem.
    Doctor(DoctorArgs),
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

/// The human table (`list`): slug plus the prefix defaults a user can
/// hand-edit — runner, graphics, Windows version.
fn render_prefix_list(prefixes: &[Prefix], json: bool) -> anyhow::Result<String> {
    if json {
        return Ok(serde_json::to_string_pretty(prefixes)?);
    }
    let mut rows: Vec<[String; 4]> = prefixes
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
    rows.insert(
        0,
        [
            "Slug".to_owned(),
            "Runner".to_owned(),
            "Graphics".to_owned(),
            "Windows".to_owned(),
        ],
    );
    let widths: Vec<usize> = (0..4)
        .map(|col| rows.iter().map(|r| r[col].len()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        let cells: Vec<String> = (0..4)
            .map(|col| format!("{:width$}", row[col], width = widths[col]))
            .collect();
        out.push_str(&cells.join("  "));
        out.push('\n');
        if i == 0 {
            out.push('\n');
        }
    }
    Ok(out)
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
    if health.missing_files.is_empty() && health.invalid_files.is_empty() {
        writeln!(out, "  ✓ files: settings.toml and every entry parse")?;
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

    use cellar_core::PrefixDefaults;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use cellar_app::PrefixService;

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
            schema_version: 1,
        };
        let out = render_tree_health(&healthy, false)?;
        assert!(out.contains("✓"), "healthy tree should pass:\n{out}");
        let mut broken = healthy;
        broken
            .invalid_files
            .push(PathBuf::from("prefixes/x/prefix.toml"));
        let out = render_tree_health(&broken, false)?;
        assert!(out.contains('✗'), "broken tree should fail:\n{out}");
        assert!(out.contains("never overwrites"), "fix hint missing:\n{out}");
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
