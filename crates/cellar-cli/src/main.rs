//! The Cellar CLI — presentation, composition root (blueprint §8, ADR 0004).
//!
//! This binary is the *only* place concrete infra is instantiated: it builds
//! the tree adapter ([`TreeStore`]) and injects it into the application
//! services, which are generic over the `core` ports. Symmetric with the
//! future `cellar-gui` leaf; flipping primary is `default-members`, zero
//! edits below presentation.
//!
//! Surface for this slice (#32): the guided TTY flow completes — step 1's
//! prefix pick-or-create is now interactive (list existing, name a new
//! one, empty line = `default`), asked only when stdin is a TTY, no
//! `--no-input`, and no `--prefix` flag; every prompt keeps its flag
//! equivalent. The "Open with Cellar" popup is this same entrypoint (ADR
//! 0004): the MIME association's exec line calls `cellar install <path>`
//! directly — no popup binary, no shell wrapper — and the no-TTY
//! invocation drives everything from the filename hint plus flags, never
//! a silent guess. (From #31: the artifact branch is decided by filename
//! hint and asked once on a TTY; the candidates the installer/archive
//! branches discovered are reviewed — interactive keep/hide/manual-add
//! with a y/N confirmation, or the `--keep` / `--keep-all` / `--add` flag
//! equivalents; confirmed entries register bound to the session's one
//! prefix; the summary lists them with `cellar launch <slug>`.)
//!
//! Desktop integration lands with #33: the composition root injects the
//! concrete [`DesktopService`] (tree root + this binary) into the install
//! service and the re-derivation use-case — registration derives the
//! app's launcher entry and cached icon, uninstall removes them, a rename
//! refreshes the entry file name, and `cellar desktop sync` re-derives
//! everything from the tree (stale entries pruned, the Open-with-Cellar
//! association wired) — an extension under the noun-group rule (ADR
//! 0004). On top of #29's `cellar launch <app>`, #27's
//! install/list/uninstall, #26's `cellar prefix …` and `cellar doctor`.
//! Exit codes (ADR 0004): 0 success, 1 operation error or doctor
//! problems, 2 usage; `launch` propagates the game's exit code raw (§7),
//! the collision with 1 documented, not mapped.

use clap::{Args, Parser, Subcommand};

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, ExitStatus};
use std::str::FromStr;

use cellar_app::{
    ArtifactKind, DesktopSync, DoctorService, InstallOutcome, InstallResult, InstallService,
    LaunchApp, LaunchMode, ListedEntry, PrefixService, RunnerService,
};
use cellar_core::ports::{__sealed, RunnerResolver, Storage};
use cellar_core::{
    AppEntry, AppKind, Candidate, LaunchPlan, Prefix, ProviderMode, ResolveError, ResolvedRunner,
    RunnerFamily, RunnerInstall, RunnerRef, RunnerSpec, TreeHealth,
};
use cellar_desktop::DesktopService;
use cellar_providers::{all_managed, all_resolvers, steam_protons, wrappers_for};
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
    /// Install a Windows artifact — standalone exe, installer, or archive
    /// (three-armed handling, blueprint §8).
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
    /// The desktop integration noun group: the re-derivation sweep that
    /// rebuilds launcher entries, icons, and the file association from
    /// the tree (blueprint §6: everything derived is re-derivable) —
    /// an extension under the noun-group rule recorded in ADR 0004.
    Desktop(DesktopArgs),
    /// Manage Cellar's runners (blueprint §8: lifecycle objects get noun
    /// groups): managed installs (fetch → verify → extract → record) and
    /// the merged managed + discover-only list.
    Runner(RunnerArgs),
    /// Sectioned capability checks with fix hints; exits 1 on any problem.
    Doctor(DoctorArgs),
}

/// The `cellar install` flags (blueprint §8): every interactive prompt has
/// a flag equivalent, so `--no-input` can script any install.
#[derive(Debug, Args)]
struct InstallArgs {
    /// Path to the Windows artifact to install.
    path: PathBuf,
    /// Prefix to bind the entry to; created when missing. Accepts the
    /// same names the prompt does: a valid slug passes through, a human
    /// name is slugified and reuses the existing prefix it names. Absent
    /// on a TTY: interactive pick-or-create — the existing prefixes are
    /// listed, a new one is named, and the empty line is `default` (the
    /// blueprint §8 default). Absent without a TTY: `default`.
    #[arg(long)]
    prefix: Option<String>,
    /// Display name for a new entry (default: the exe's file name).
    #[arg(long)]
    name: Option<String>,
    /// Entry kind — drives the defaults-floor preset hook (games → Proton,
    /// tools → wine).
    #[arg(long, value_parser = AppKind::from_str, default_value = "game")]
    kind: AppKind,
    /// How to handle the artifact: standalone registers without executing;
    /// installer runs inside the prefix with its exit awaited; archive
    /// extracts into the prefix. Absent: hinted from the file name as the
    /// prompt's default (e.g. setup.exe → installer, .zip → archive) — the
    /// branch is asked once on a TTY, never silently fixed.
    #[arg(long, value_parser = ArtifactKind::from_str)]
    artifact: Option<ArtifactKind>,
    /// Do not prompt — every decision must come from flags; unconfirmed
    /// candidates register nothing.
    #[arg(long)]
    no_input: bool,
    /// Keep the given candidates (the numbers printed after the
    /// run/extract); the rest stay hidden. Repeatable.
    #[arg(long)]
    keep: Vec<usize>,
    /// Keep every candidate discovery found.
    #[arg(long, conflicts_with = "keep")]
    keep_all: bool,
    /// Manually add an executable discovery missed (repeatable).
    #[arg(long)]
    add: Vec<PathBuf>,
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

#[derive(Debug, Args)]
struct DesktopArgs {
    #[command(subcommand)]
    command: DesktopCommand,
}

#[derive(Debug, Subcommand)]
enum DesktopCommand {
    /// Re-derive the launcher entries, icons, and the Open-with-Cellar
    /// association from the tree (blueprint §6: the cache is disposable,
    /// everything derived is rebuilt), and remove stale entries left by
    /// renamed or removed apps. The sweep reads app state only — it
    /// never writes back into the tree.
    Sync,
}

#[derive(Debug, Args)]
struct RunnerArgs {
    #[command(subcommand)]
    command: RunnerCommand,
}

#[derive(Debug, Subcommand)]
enum RunnerCommand {
    /// Install one managed runner version: download (resumable from the
    /// disposable cache), verify against the provider's published
    /// SHA-512 when there is one (a corrupt download fails closed),
    /// extract, probe, and record in the authoritative inventory
    /// (`runtime/providers.toml`) — the runtime dir is rebuildable from
    /// it. Idempotent: an installed version is a no-op.
    Install {
        /// The provider's identifier (`proton`, `umu`).
        provider: String,
        /// The version to install — the release tag, e.g.
        /// `GE-Proton11-5` (no guessed "latest": the artifact is named
        /// deterministically by its tag).
        version: String,
    },
    /// List every runner: managed installs (the inventory) and
    /// discover-only host state (wine and umu-run on PATH, Steam Proton
    /// in Steam's compatibility layout). Discover-only entries are
    /// read-only — Cellar never modifies them.
    List {
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
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

/// The `cellar desktop sync` handler: re-derive every launcher entry and
/// icon from the tree (blueprint §6: the cache is disposable), prune the
/// stale entry files a rename or uninstall left behind, and wire the
/// Open-with-Cellar association — the re-derivation `cellar list` and
/// `cellar doctor` rely on, one-way (tree state is only read).
fn run_desktop_sync(store: &TreeStore, desktop: &DesktopService) -> anyhow::Result<ExitCode> {
    let report = DesktopSync::new(store.clone(), desktop.clone()).sync()?;
    println!(
        "Synced {} launcher entries ({} with icons)",
        report.entries, report.icons
    );
    if report.removed_entries.is_empty() {
        println!("No stale entries to remove");
    } else {
        for path in &report.removed_entries {
            println!("Removed stale entry {}", path.display());
        }
    }
    println!("Wired the Open-with-Cellar file association");
    Ok(ExitCode::SUCCESS)
}

/// The registry's resolver composite over one store — the composition
/// root wiring: providers scan the store's own `runtime/` dir (managed
/// installs), never a guessed data root.
fn resolvers_for(store: &TreeStore) -> ResolverSet {
    ResolverSet::new(all_resolvers(&store.data_root().join("runtime")))
}

/// The `cellar runner install` handler (blueprint §5, §8): the provider's
/// manifest — one descriptor, zero pipeline code at the surface — drives
/// the storage-owned pipeline: fetch (resumable), verify (SHA-512 when
/// the manifest names a checksum source), extract, probe, record — and
/// the runtime dir is rebuildable from the recorded inventory (AC).
fn run_runner_install(
    store: &TreeStore,
    provider: &str,
    version: &str,
) -> anyhow::Result<ExitCode> {
    let known: Vec<String> = all_managed()
        .iter()
        .map(|runner| runner.manifest().provider_id.clone())
        .collect();
    let manifest = all_managed()
        .into_iter()
        .find(|runner| runner.manifest().provider_id == provider)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown runner provider {provider:?} — managed providers: {}",
                known.join(", ")
            )
        })?;
    let service = RunnerService::new(store.clone());
    let dir = service.install(manifest.manifest(), version)?;
    println!("Installed {provider} {version} at {}", dir.display());
    Ok(ExitCode::SUCCESS)
}

/// One `runner list` row (blueprint §8): managed or discover-only, with
/// the resolved version and path. The presentation's row shape — the
/// `--json` contract is audited in the surface sweep (#36).
#[derive(serde::Serialize)]
struct RunnerRow {
    mode: &'static str,
    provider: String,
    version: Option<String>,
    path: PathBuf,
}

/// The `cellar runner list` handler: managed rows from the authoritative
/// inventory plus discover-only host state — wine and umu-run on PATH,
/// Steam Proton in Steam's compatibility layout — each resolved through
/// the same registry the launch pipeline uses. Discover-only entries are
/// read-only: Cellar never modifies them.
fn run_runner_list(store: &TreeStore, json: bool) -> anyhow::Result<ExitCode> {
    let service = RunnerService::new(store.clone());
    let inventory = service.installed()?;
    let resolvers = resolvers_for(store);
    let mut managed = Vec::new();
    for record in inventory {
        managed.push(RunnerRow {
            mode: "managed",
            provider: record.provider_id,
            version: Some(record.version),
            path: store.data_root().join("runtime").join(&record.install),
        });
    }
    let mut discovered = Vec::new();
    for family in [RunnerFamily::Wine, RunnerFamily::Umu] {
        if let Ok(resolved) = resolvers.resolve(&RunnerSpec::new(family)) {
            // A managed-capable family may resolve to its managed
            // install; the list shows only its read-only state (the
            // managed row already carries the install).
            if let RunnerInstall::Discovered { path, version } = &resolved.reference.install {
                discovered.push(RunnerRow {
                    mode: "discover-only",
                    provider: resolved.reference.provider_id,
                    version: version.clone(),
                    path: path.clone(),
                });
            }
        }
    }
    for proton in steam_protons() {
        discovered.push(RunnerRow {
            mode: "discover-only",
            provider: "proton".to_owned(),
            version: Some(proton.version),
            path: proton.dir,
        });
    }
    if json {
        let mut all = managed;
        all.extend(discovered);
        print!("{}", serde_json::to_string_pretty(&all)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!("Managed:");
    for row in &managed {
        println!(
            "  {:<8} {:<20} {}",
            row.provider,
            row.version.as_deref().unwrap_or("-"),
            row.path.display()
        );
    }
    println!("Discover-only (read-only — Cellar never modifies these):");
    for row in &discovered {
        println!(
            "  {:<8} {:<20} {}",
            row.provider,
            row.version.as_deref().unwrap_or("-"),
            row.path.display()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn run() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    let store = TreeStore::from_env()?;
    // The composition root builds the desktop adapter once: over the
    // tree root (the entries live beside it, the icons under its
    // disposable cache) and this binary — the Exec target of every
    // launcher entry and the popup's install entrypoint (#32/#33).
    let desktop = DesktopService::new(
        store.data_root().to_path_buf(),
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cellar")),
    );
    match cli.command {
        Command::Install(args) => run_install(&store, &desktop, &args),
        Command::List(args) => {
            let service = InstallService::new(store.clone(), resolvers_for(&store), desktop);
            let entries = service.list()?;
            print!("{}", render_app_list(&entries, args.json)?);
            Ok(ExitCode::SUCCESS)
        }
        Command::Launch(args) => {
            let service = LaunchApp::with_chain(store.clone(), resolvers_for(&store), wrappers_for);
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
            let service = InstallService::new(store.clone(), resolvers_for(&store), desktop);
            service.uninstall(&args.slug)?;
            // Glossary: Uninstall — entry removal for now; Cellar never
            // deletes the app's own files.
            println!(
                "Uninstalled '{}' — entry removed; Cellar never deletes the app's own files",
                args.slug
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Desktop(args) => match args.command {
            DesktopCommand::Sync => run_desktop_sync(&store, &desktop),
        },
        Command::Runner(args) => match args.command {
            RunnerCommand::Install { provider, version } => {
                run_runner_install(&store, &provider, &version)
            }
            RunnerCommand::List { json } => run_runner_list(&store, json),
        },
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

/// The `cellar install` handler — the flagship flow (blueprint §8): bind
/// or create the prefix — the flag, or the interactive pick-or-create on a
/// TTY, or the `default` default (step 1) — handle the artifact (branch
/// hinted from the file name, asked once on a TTY, never silently), review
/// the discovered candidates (interactive keep/hide/manual-add with a y/N
/// confirmation when stdin is a TTY and no decision flags are given;
/// `--keep`/`--keep-all`/`--add` otherwise), register the confirmed
/// entries bound to the session's one prefix, and print the summary with
/// the next command. Nothing registers without confirmation: an
/// unreviewed candidate list registers nothing. The same handler is the
/// "Open with Cellar" popup entrypoint (#32): a file-manager exec line
/// calls `cellar install <path>` directly — no TTY, no wrapper binary —
/// and the no-prompt path is exactly the flag/hint path below. Every
/// registration also derives the app's launcher entry and icon through
/// the injected desktop adapter (#33) — created here, removed on
/// uninstall, renamed along with the app.
fn run_install(
    store: &TreeStore,
    desktop: &DesktopService,
    args: &InstallArgs,
) -> anyhow::Result<ExitCode> {
    // Interactive iff stdin is a TTY and `--no-input` is absent
    // (blueprint §8) — and only when the decision flags leave nothing to
    // ask (a given `--prefix`/`--artifact`/`--keep`/`--keep-all`/`--add`
    // skips its prompt).
    let interactive = std::io::stdin().is_terminal() && !args.no_input;
    let service = InstallService::with_chain(
        store.clone(),
        resolvers_for(store),
        desktop.clone(),
        wrappers_for,
    );
    let prefix = resolve_prefix_slug(args.prefix.as_deref(), interactive, store)?;
    let artifact = match args.artifact {
        Some(kind) => kind,
        None if interactive => prompt_artifact_kind(&args.path)?,
        // The filename hint is only the question's default (blueprint §8);
        // used without the prompt, the guess is announced — never silent,
        // always overridable.
        None => {
            let hint = artifact_hint(&args.path);
            eprintln!(
                "Treating '{}' as {} (hint from the file name — pass --artifact to override)",
                file_name(&args.path),
                hint.as_str()
            );
            hint
        }
    };
    let outcome = service.install(
        &args.path,
        &prefix,
        args.name.as_deref(),
        args.kind,
        artifact,
    )?;
    match artifact {
        // The flagship summary (blueprint §8 step 4): what was registered,
        // plus the next command.
        ArtifactKind::Standalone => {
            let result = outcome
                .registrations
                .first()
                .expect("a standalone session registers exactly once");
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
        }
        ArtifactKind::Installer => {
            println!(
                "Ran installer '{}' in prefix '{}'",
                file_name(&args.path),
                outcome.prefix_slug
            );
            if let Some(log) = &outcome.log_path {
                println!("output: {}", log.display());
            }
            review_and_register(&service, &outcome, args, interactive)?;
        }
        ArtifactKind::Archive => {
            println!(
                "Extracted '{}' into prefix '{}'",
                file_name(&args.path),
                outcome.prefix_slug
            );
            review_and_register(&service, &outcome, args, interactive)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The review step of a running/extracting session (blueprint §8 step 3):
/// present the discovered candidates, apply the keep/hide/manual-add
/// decisions — from the prompts on a TTY, or from the `--keep` /
/// `--keep-all` / `--add` flags — confirm when the decisions came from
/// prompts, register the confirmed entries, and print the summary (step
/// 4). Nothing registers without confirmation: with no flags, an
/// unreviewed list registers zero entries.
fn review_and_register(
    service: &InstallService<TreeStore, ResolverSet, DesktopService>,
    outcome: &InstallOutcome,
    args: &InstallArgs,
    interactive: bool,
) -> anyhow::Result<()> {
    let decided = !args.keep.is_empty() || args.keep_all || !args.add.is_empty();
    let (keep, add) = if interactive && !decided {
        interactive_review(&outcome.candidates)?
    } else {
        print_candidates(&outcome.candidates);
        (select_kept(&outcome.candidates, args)?, args.add.clone())
    };
    // The y/N gate exists only for prompted decisions — flags are the
    // scripted confirmation (blueprint §8: every prompt has a flag).
    let confirmed = keep.len() + add.len();
    if confirmed > 0
        && interactive
        && !decided
        && !confirm_registration(confirmed, &outcome.prefix_slug)?
    {
        println!("Registered nothing — the confirmation was declined");
        return Ok(());
    }
    let registrations = service.register_reviewed(outcome, &keep, &add, args.kind)?;
    print!(
        "{}",
        registration_summary(&registrations, &outcome.prefix_slug)
    );
    Ok(())
}

/// The artifact-kind question's filename-hint default (blueprint §8: the
/// branch has a filename-hint default and is asked once — never silently
/// fixed): `.zip` hints archive, an installer-ish name hints installer,
/// everything else hints standalone.
fn artifact_hint(path: &Path) -> ArtifactKind {
    let name = file_name(path).to_ascii_lowercase();
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
    {
        return ArtifactKind::Archive;
    }
    if name.contains("setup") || name.contains("install") {
        return ArtifactKind::Installer;
    }
    ArtifactKind::Standalone
}

/// Parse a keep-selection line: whitespace/comma-separated 1-based
/// candidate numbers, each within `count`. Empty input selects nothing.
fn parse_keep_indices(input: &str, count: usize) -> Result<Vec<usize>, String> {
    let mut indices = Vec::new();
    for token in input.split([',', ' ', '\t']).filter(|t| !t.is_empty()) {
        let index: usize = token
            .parse()
            .map_err(|_| format!("{token:?} is not a candidate number — enter numbers like 1,3"))?;
        if index == 0 || index > count {
            return Err(format!(
                "candidate {index} is out of range (the review listed 1..={count})"
            ));
        }
        indices.push(index - 1);
    }
    Ok(indices)
}

/// Parse the artifact-kind choice line; empty input picks the default.
/// Accepts the numbered choice (1/2/3) and the vocabulary word.
fn parse_artifact_choice(input: &str, default: ArtifactKind) -> Option<ArtifactKind> {
    match input.trim().to_ascii_lowercase().as_str() {
        "" => Some(default),
        "1" | "i" | "installer" => Some(ArtifactKind::Installer),
        "2" | "a" | "archive" => Some(ArtifactKind::Archive),
        "3" | "s" | "standalone" => Some(ArtifactKind::Standalone),
        _ => None,
    }
}

/// One pick from the prefix pick-or-create prompt (blueprint §8 step 1):
/// an existing prefix by its listed number, or the name of a new one —
/// the empty line is the `default` default. The resolution against the
/// listed prefixes happens in [`resolve_prefix_pick`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum PrefixPick {
    /// The zero-based index of a listed existing prefix.
    Existing(usize),
    /// The name of a new prefix (slugified on resolution).
    New(String),
}

/// Parse a pick-or-create line: empty input names the new prefix
/// `default` (the blueprint §8 default, also the `--prefix` default), a
/// number picks the listed existing prefix (1-based), anything else is a
/// name for a new one.
fn parse_prefix_choice(input: &str, count: usize) -> Result<PrefixPick, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(PrefixPick::New("default".to_owned()));
    }
    if let Ok(index) = trimmed.parse::<usize>() {
        if index == 0 || index > count {
            return Err(format!(
                "prefix {index} is out of range (the list showed 1..={count})"
            ));
        }
        return Ok(PrefixPick::Existing(index - 1));
    }
    Ok(PrefixPick::New(trimmed.to_owned()))
}

/// Resolve a pick against the listed prefixes to the slug the session
/// binds: an existing entry by its number; a name — slugified, reusing the
/// existing prefix its slug names (typing "My Games" picks `my-games`),
/// or the slug the session creates for the new one.
fn resolve_prefix_pick(pick: PrefixPick, prefixes: &[Prefix]) -> Result<String, String> {
    match pick {
        PrefixPick::Existing(index) => prefixes
            .get(index)
            .map(|prefix| prefix.slug.clone())
            .ok_or_else(|| {
                format!(
                    "prefix {index} is out of range (the list showed 1..={})",
                    prefixes.len()
                )
            }),
        PrefixPick::New(name) => resolve_prefix_name(&name, prefixes),
    }
}

/// Resolve a prefix name the way the prompt does: slugified (blueprint §6
/// naming), reusing the existing prefix its slug names, or the slug the
/// session will create. A name that already is a valid slug passes through
/// unchanged — the `--prefix` flag and the prompt accept the same
/// vocabulary, because every prompt has a flag equivalent.
fn resolve_prefix_name(name: &str, prefixes: &[Prefix]) -> Result<String, String> {
    let slug = cellar_core::slug::slugify(name);
    if slug.is_empty() {
        return Err(format!("cannot form a prefix slug from {name:?}"));
    }
    if let Some(existing) = prefixes.iter().find(|prefix| prefix.slug == slug) {
        return Ok(existing.slug.clone());
    }
    Ok(slug)
}

/// The candidates the review's flags keep: `--keep-all` keeps every one;
/// `--keep N…` keeps the printed numbers (validated against the list,
/// repeated numbers kept once); no flags keep nothing — the review never
/// registers without confirmation.
fn select_kept(candidates: &[Candidate], args: &InstallArgs) -> anyhow::Result<Vec<Candidate>> {
    if args.keep_all {
        return Ok(candidates.to_vec());
    }
    let mut kept: Vec<Candidate> = Vec::new();
    for &index in &args.keep {
        if index == 0 || index > candidates.len() {
            anyhow::bail!(
                "candidate {index} is out of range — the review printed 1..={}",
                candidates.len()
            );
        }
        let candidate = &candidates[index - 1];
        if !kept.iter().any(|kept| kept.exe == candidate.exe) {
            kept.push(candidate.clone());
        }
    }
    Ok(kept)
}

/// The session summary (blueprint §8 step 4): what was registered —
/// created vs updated — and the next command per entry. Zero
/// registrations is a valid session outcome (an empty review); it says so
/// plainly rather than pretending.
fn registration_summary(registrations: &[InstallResult], prefix_slug: &str) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    if registrations.is_empty() {
        writeln!(
            out,
            "Registered nothing in prefix '{prefix_slug}' — no entries were confirmed; \
             re-run `cellar install` to review the candidates, or pass --keep/--keep-all/--add"
        )
        .expect("writing to a String cannot fail");
        return out;
    }
    let created = registrations
        .iter()
        .filter(|result| !result.was_update)
        .count();
    let updated = registrations.len() - created;
    // One header line, told apart: everything fresh, everything an update,
    // or the mixed session.
    if updated == 0 {
        let noun = if created == 1 { "entry" } else { "entries" };
        writeln!(
            out,
            "Registered {created} {noun} in prefix '{prefix_slug}':"
        )
        .expect("write");
    } else if created == 0 {
        let noun = if updated == 1 { "entry" } else { "entries" };
        writeln!(
            out,
            "Updated {updated} existing {noun} in prefix '{prefix_slug}':"
        )
        .expect("write");
    } else {
        let noun = if created == 1 { "entry" } else { "entries" };
        writeln!(
            out,
            "Registered {created} {noun} in prefix '{prefix_slug}':"
        )
        .expect("write");
        let noun = if updated == 1 { "entry" } else { "entries" };
        writeln!(out, "Updated {updated} existing {noun}").expect("write");
    }
    for result in registrations {
        writeln!(
            out,
            "  {} — {} ({})",
            result.entry.slug,
            result.entry.exe.display(),
            result.entry.kind.as_str()
        )
        .expect("write");
    }
    writeln!(out, "Run it with:").expect("write");
    for result in registrations {
        writeln!(out, "  cellar launch {}", result.entry.slug).expect("write");
    }
    out
}

/// The interactive keep/hide/manual-add review (blueprint §8 step 3 — the
/// TTY presentation): the candidates are printed numbered, the user keeps
/// by number (the rest are hidden) and may add exes the scan missed; an
/// empty line ends the review. The manual-add offer is always made — when
/// discovery found nothing, that is exactly when a manual add is needed
/// (glossary: manually adding a candidate discovery missed). Everything is
/// re-askable on bad input; nothing is registered here — the y/N
/// confirmation in [`review_and_register`] gates the writes.
fn interactive_review(candidates: &[Candidate]) -> anyhow::Result<(Vec<Candidate>, Vec<PathBuf>)> {
    print_candidates(candidates);
    let keep = if candidates.is_empty() {
        Vec::new()
    } else {
        loop {
            print!(
                "Keep which candidates? (numbers, e.g. 1,3 — the rest are hidden; empty = none): "
            );
            flush_stdout()?;
            let line = read_line()?;
            match parse_keep_indices(&line, candidates.len()) {
                Ok(indices) => break indices.into_iter().map(|i| candidates[i].clone()).collect(),
                Err(message) => eprintln!("cellar: {message}"),
            }
        }
    };
    let mut add = Vec::new();
    loop {
        print!("Manually add an exe the scan missed? (path, or empty when done): ");
        flush_stdout()?;
        let line = read_line()?;
        if line.trim().is_empty() {
            break;
        }
        add.push(PathBuf::from(line.trim()));
    }
    Ok((keep, add))
}

/// The session's prefix slug (blueprint §8 step 1): the `--prefix` flag's
/// answer when given, the interactive pick-or-create's answer when the
/// session can prompt, and the `default` default otherwise. The flag
/// resolves exactly like the prompt (a name is slugified and reuses the
/// existing prefix it names), so the flag equivalent is faithful — and the
/// non-interactive popup path can express everything the TTY path accepts.
fn resolve_prefix_slug(
    flag: Option<&str>,
    interactive: bool,
    store: &TreeStore,
) -> anyhow::Result<String> {
    let prefixes = PrefixService::new(store.clone()).list()?;
    match flag {
        Some(name) => resolve_prefix_name(name, &prefixes).map_err(anyhow::Error::msg),
        None if interactive => prompt_prefix(store),
        None => Ok("default".to_owned()),
    }
}

/// The interactive prefix pick-or-create (blueprint §8 step 1): the
/// existing prefixes are listed, the user picks one by number or names a
/// new one — the empty line is `default`. A typed name reuses the existing
/// prefix it slugifies to; a genuinely new name is created by the session
/// itself (the install binds-or-creates, dedupe-safe). Re-asked on bad
/// input; guarded by the caller's `interactive` gate — never reached
/// without a TTY, and skipped entirely when `--prefix` decided.
fn prompt_prefix(store: &TreeStore) -> anyhow::Result<String> {
    let service = PrefixService::new(store.clone());
    let prefixes = service.list()?;
    loop {
        if prefixes.is_empty() {
            print!("No prefixes yet — name the new one (default `default`): ");
        } else {
            println!("Existing prefixes:");
            for (index, prefix) in prefixes.iter().enumerate() {
                println!("  {}. {}", index + 1, prefix.slug);
            }
            print!("Use an existing prefix or name a new one (default `default`): ");
        }
        flush_stdout()?;
        let line = read_line()?;
        match parse_prefix_choice(&line, prefixes.len()) {
            Ok(pick) => match resolve_prefix_pick(pick, &prefixes) {
                Ok(slug) => return Ok(slug),
                Err(message) => eprintln!("cellar: {message}"),
            },
            Err(message) => eprintln!("cellar: {message}"),
        }
    }
}

/// The artifact-kind question: the three branches with the filename-hint
/// default (blueprint §8: asked once, never guessed) — looped until a
/// recognizable answer.
fn prompt_artifact_kind(path: &Path) -> anyhow::Result<ArtifactKind> {
    let hint = artifact_hint(path);
    eprintln!("How should Cellar handle '{}'?", file_name(path));
    eprintln!("  1. installer — run it inside the prefix and discover what it drops");
    eprintln!("  2. archive — extract it into the prefix");
    eprintln!("  3. standalone — register without executing");
    loop {
        print!("Choice (1-3, default {}): ", hint.as_str());
        flush_stdout()?;
        match parse_artifact_choice(&read_line()?, hint) {
            Some(kind) => return Ok(kind),
            None => eprintln!("cellar: enter 1 (installer), 2 (archive), or 3 (standalone)"),
        }
    }
}

/// The y/N registration gate for prompted decisions ("nothing registers
/// without confirmation", blueprint §8).
fn confirm_registration(count: usize, prefix_slug: &str) -> anyhow::Result<bool> {
    let noun = if count == 1 { "entry" } else { "entries" };
    loop {
        print!("Register {count} {noun} in prefix '{prefix_slug}'? [y/N]: ");
        flush_stdout()?;
        match read_line()?.trim().to_ascii_lowercase().as_str() {
            "" | "n" | "no" => return Ok(false),
            "y" | "yes" => return Ok(true),
            _ => eprintln!("cellar: answer y or n"),
        }
    }
}

/// Flush the prompt to the terminal before blocking on input.
fn flush_stdout() -> anyhow::Result<()> {
    use std::io::Write;
    std::io::stdout().flush()?;
    Ok(())
}

/// One line of prompt input, with the trailing newline removed.
fn read_line() -> anyhow::Result<String> {
    use std::io::BufRead;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

/// The last path component for presentation, or the whole path when it has
/// none (e.g. `install /tmp/setup.exe` → `setup.exe`).
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The discovery review preview (blueprint §8 step 3): the numbered
/// candidate list both presentation modes print — the interactive prompt
/// reuses the numbers, and the `--keep` flags reference them.
fn print_candidates(candidates: &[Candidate]) {
    if candidates.is_empty() {
        println!("No executable candidates in the prefix's menu/desktop areas");
        return;
    }
    println!("Executable candidates found in the prefix's menu/desktop areas:");
    for (index, candidate) in candidates.iter().enumerate() {
        println!(
            "  {}. {} — {}",
            index + 1,
            candidate.label,
            candidate.exe.display()
        );
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

    use cellar_app::{
        EntryStatus, InstallOutcome, InstallResult, InstallService, ListedEntry, PrefixService,
    };
    use cellar_core::{
        AppEntry, AppKind, Overrides, Prefix, PrefixDefaults, RunnerFamily, RunnerInstall,
        RunnerRef,
    };

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    /// The one registration of a standalone session — e2e tests unwrap it
    /// (sessions may register many once review lands, #31).
    fn only_registration(outcome: InstallOutcome) -> InstallResult {
        let mut registrations = outcome.registrations;
        assert_eq!(registrations.len(), 1, "expected exactly one registration");
        registrations.pop().expect("length asserted above")
    }

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
        assert!(
            args.prefix.is_none(),
            "the prefix is decided by the prompt or defaults to `default` — \
             the flag is absent unless given"
        );
        assert!(
            args.name.is_none(),
            "the name defaults to the exe file name"
        );
        assert_eq!(args.kind, AppKind::Game, "the default kind is `game`");
        assert!(
            args.artifact.is_none(),
            "the branch is hinted from the file name, never silently fixed"
        );
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
        assert_eq!(args.prefix.as_deref(), Some("games"));
        assert_eq!(args.name.as_deref(), Some("Balatro"));
        assert_eq!(args.kind, AppKind::Tool);
    }

    #[test]
    fn parses_the_artifact_branch_flag() {
        for (flag, expected) in [
            ("installer", ArtifactKind::Installer),
            ("archive", ArtifactKind::Archive),
            ("standalone", ArtifactKind::Standalone),
        ] {
            let cli = Cli::try_parse_from(["cellar", "install", "x.exe", "--artifact", flag])
                .unwrap_or_else(|e| panic!("parse --artifact {flag}: {e}"));
            let Command::Install(args) = cli.command else {
                panic!("unexpected command");
            };
            assert_eq!(args.artifact, Some(expected));
        }
        assert!(
            Cli::try_parse_from(["cellar", "install", "x.exe", "--artifact", "bundle"]).is_err(),
            "an unknown artifact branch is a usage error"
        );
    }

    #[test]
    fn parses_the_review_flags() {
        let cli = Cli::try_parse_from([
            "cellar",
            "install",
            "x.exe",
            "--artifact",
            "installer",
            "--no-input",
            "--keep",
            "1",
            "--keep",
            "3",
            "--add",
            "/prefix/drive_c/tools/helper.exe",
        ])
        .unwrap_or_else(|e| panic!("parse: {e}"));
        let Command::Install(args) = cli.command else {
            panic!("unexpected command");
        };
        assert!(args.no_input, "--no-input suppresses every prompt");
        assert_eq!(
            args.keep,
            [1, 3],
            "keep takes the printed candidate numbers"
        );
        assert_eq!(
            args.add,
            [PathBuf::from("/prefix/drive_c/tools/helper.exe")],
            "add takes manual exe paths"
        );
        assert!(!args.keep_all);
        let cli = Cli::try_parse_from([
            "cellar",
            "install",
            "x.exe",
            "--artifact",
            "archive",
            "--keep-all",
        ])
        .unwrap_or_else(|e| panic!("parse --keep-all: {e}"));
        let Command::Install(args) = cli.command else {
            panic!("unexpected command");
        };
        assert!(args.keep_all);
        // --keep-all with an explicit --keep selection is contradictory.
        assert!(
            Cli::try_parse_from(["cellar", "install", "x.exe", "--keep-all", "--keep", "1"])
                .is_err(),
            "--keep-all conflicts with --keep"
        );
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
        let service = InstallService::new(
            store.clone(),
            ResolverSet::new(all_resolvers(&store.data_root().join("runtime"))),
            test_desktop(&store),
        );
        let exe = root.join("drive_c/My Game.exe");
        std::fs::create_dir_all(exe.parent().unwrap_or(Path::new(".")))
            .unwrap_or_else(|e| panic!("mkdir: {e}"));
        std::fs::write(&exe, "MZ").unwrap_or_else(|e| panic!("write: {e}"));
        let first = only_registration(service.install(
            &exe,
            "default",
            None,
            AppKind::Game,
            ArtifactKind::Standalone,
        )?);
        assert!(!first.was_update);
        assert_eq!(first.entry.slug, "my-game");
        assert_eq!(
            first.entry.exe,
            std::fs::canonicalize(&exe).unwrap_or_else(|e| panic!("canonicalize: {e}")),
            "identity is the canonical exe path"
        );
        let second = only_registration(service.install(
            &exe,
            "default",
            None,
            AppKind::Tool,
            ArtifactKind::Standalone,
        )?);
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
        let registered = only_registration(
            InstallService::new(
                store.clone(),
                ResolverSet::new(all_resolvers(&store.data_root().join("runtime"))),
                test_desktop(&store),
            )
            .install(
                &exe,
                "default",
                Some("My Tool"),
                AppKind::Tool,
                ArtifactKind::Standalone,
            )?,
        );
        let mut prefix = store.load_prefix("default")?;
        prefix.defaults.runner = Some(RunnerSpec::with_configured(
            RunnerFamily::Wine,
            ConfiguredRunner::Path(wine.clone()),
        ));
        store.save_prefix(&prefix)?;
        let service = LaunchApp::new(
            store.clone(),
            ResolverSet::new(all_resolvers(&store.data_root().join("runtime"))),
        );
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
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn launch_detached_releases_the_process_and_keeps_its_log() -> anyhow::Result<()> {
        use cellar_core::ConfiguredRunner;

        // Detached: the handle returns with the process still running,
        // and its output still lands in its own per-launch log.
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-detach-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(root.clone());
        std::fs::create_dir_all(&root)?;
        let sleeper = root.join("stub-sleeper");
        write_stub_script(&sleeper, "echo sleeping\nexit 0\n")?;
        let exe = root.join("drive_c/tool.exe");
        std::fs::create_dir_all(exe.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(&exe, "MZ")?;
        let registered = only_registration(
            InstallService::new(
                store.clone(),
                ResolverSet::new(all_resolvers(&store.data_root().join("runtime"))),
                test_desktop(&store),
            )
            .install(
                &exe,
                "default",
                Some("My Tool"),
                AppKind::Tool,
                ArtifactKind::Standalone,
            )?,
        );
        let mut prefix = store.load_prefix("default")?;
        prefix.defaults.runner = Some(RunnerSpec::with_configured(
            RunnerFamily::Wine,
            ConfiguredRunner::Path(sleeper.clone()),
        ));
        store.save_prefix(&prefix)?;
        let service = LaunchApp::new(
            store.clone(),
            ResolverSet::new(all_resolvers(&store.data_root().join("runtime"))),
        );
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
        let registered = only_registration(
            InstallService::new(
                store.clone(),
                ResolverSet::new(all_resolvers(&store.data_root().join("runtime"))),
                test_desktop(&store),
            )
            .install(
                &exe,
                "default",
                Some("My Tool"),
                AppKind::Tool,
                ArtifactKind::Standalone,
            )?,
        );
        let mut prefix = store.load_prefix("default")?;
        prefix.defaults.runner = Some(RunnerSpec::with_configured(
            RunnerFamily::Wine,
            ConfiguredRunner::Path(wine),
        ));
        store.save_prefix(&prefix)?;
        // Plan it: resolve → check → plan, pure and printable.
        let app = LaunchApp::new(
            store.clone(),
            ResolverSet::new(all_resolvers(&store.data_root().join("runtime"))),
        );
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
        let game = only_registration(
            InstallService::new(
                store.clone(),
                ResolverSet::new(all_resolvers(&store.data_root().join("runtime"))),
                test_desktop(&store),
            )
            .install(
                &game_exe,
                &games.slug,
                None,
                AppKind::Game,
                ArtifactKind::Standalone,
            )?,
        );
        let app = LaunchApp::new(
            second_store.clone(),
            ResolverSet::new(all_resolvers(&second_store.data_root().join("runtime"))),
        );
        let err = app.plan(&game.entry.slug, &[]).expect_err("no proton yet");
        assert!(
            err.to_string().contains("proton"),
            "the SuggestInstall message names the family: {err}"
        );
        Ok(())
    }

    /// Build a ZIP with the given `(name, contents)` pairs — the CLI's
    /// mirror of the fixtures cellar-app owns (a shared harness crate stays
    /// out per the locked §4 graph); kept write→close so it cannot drift.
    fn build_zip(path: &Path, entries: &[(&str, &str)]) {
        use std::io::Write;

        use zip::write::SimpleFileOptions;

        let file = std::fs::File::create(path).unwrap_or_else(|e| panic!("create: {e}"));
        let mut zip = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        for (name, contents) in entries {
            zip.start_file(*name, options)
                .unwrap_or_else(|e| panic!("start {name}: {e}"));
            zip.write_all(contents.as_bytes())
                .unwrap_or_else(|e| panic!("write {name}: {e}"));
        }
        zip.finish().unwrap_or_else(|e| panic!("finish: {e}"));
    }

    #[test]
    #[cfg(unix)]
    fn install_installer_branch_end_to_end_with_a_stub_wine() -> anyhow::Result<()> {
        use cellar_core::ConfiguredRunner;

        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-installer-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(root.clone());
        std::fs::create_dir_all(&root)?;
        // The configured stub runner plays wine; the stub installer plants
        // an exe in the prefix's Desktop area and reports success — the
        // full installer → awaited exit → discovery loop, in one binary.
        let wine = root.join("stub-wine");
        write_stub_script(
            &wine,
            "mkdir -p \"$WINEPREFIX/drive_c/users/me/Desktop\"\n\
             echo \"MZ\" > \"$WINEPREFIX/drive_c/users/me/Desktop/game.exe\"\n\
             exit 0\n",
        )?;
        let installer = root.join("setup.exe");
        std::fs::write(&installer, "MZ-setup")?;
        PrefixService::new(store.clone()).create("default")?;
        let mut prefix = store.load_prefix("default")?;
        prefix.defaults.runner = Some(RunnerSpec::with_configured(
            RunnerFamily::Wine,
            ConfiguredRunner::Path(wine),
        ));
        store.save_prefix(&prefix)?;
        let service = InstallService::new(
            store.clone(),
            ResolverSet::new(all_resolvers(&store.data_root().join("runtime"))),
            test_desktop(&store),
        );
        let outcome = service.install(
            &installer,
            "default",
            None,
            AppKind::Game,
            ArtifactKind::Installer,
        )?;
        assert!(outcome.registrations.is_empty(), "nothing registers yet");
        assert_eq!(outcome.prefix_slug, "default");
        let log = outcome.log_path.expect("the installer run has a log");
        assert!(log.starts_with(store.launch_logs_dir()));
        assert!(log.is_file());
        assert_eq!(outcome.candidates.len(), 1, "the planted exe is found");
        assert_eq!(outcome.candidates[0].label, "game");
        assert!(
            outcome.candidates[0].exe.ends_with("Desktop/game.exe"),
            "candidate path: {}",
            outcome.candidates[0].exe.display()
        );
        // A failed installer aborts the session with the raw exit code.
        let failing = root.join("stub-failing-wine");
        write_stub_script(&failing, "exit 7\n")?;
        let mut prefix = store.load_prefix("default")?;
        prefix.defaults.runner = Some(RunnerSpec::with_configured(
            RunnerFamily::Wine,
            ConfiguredRunner::Path(failing),
        ));
        store.save_prefix(&prefix)?;
        let service = InstallService::new(
            store.clone(),
            ResolverSet::new(all_resolvers(&store.data_root().join("runtime"))),
            test_desktop(&TreeStore::new(root)),
        );
        let err = service
            .install(
                &installer,
                "default",
                None,
                AppKind::Game,
                ArtifactKind::Installer,
            )
            .expect_err("failed installer aborts");
        assert!(
            matches!(
                &err,
                cellar_app::InstallError::InstallerFailed {
                    code: Some(7),
                    signal: None
                }
            ),
            "the exit code is reported raw: {err}"
        );
        assert!(err.to_string().contains("session aborted"), "{err}");
        Ok(())
    }

    #[test]
    fn install_archive_branch_end_to_end() -> anyhow::Result<()> {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-archive-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(root.clone());
        std::fs::create_dir_all(&root)?;
        let bundle = root.join("bundle.zip");
        build_zip(
            &bundle,
            &[("game/Game.exe", "MZ"), ("game/data/level.bin", "level")],
        );
        let service = InstallService::new(
            store.clone(),
            ResolverSet::new(all_resolvers(&store.data_root().join("runtime"))),
            test_desktop(&store),
        );
        let outcome = service.install(
            &bundle,
            "default",
            None,
            AppKind::Game,
            ArtifactKind::Archive,
        )?;
        // The archive landed at the prefix's wine root, structure intact.
        let drive_c = root.join("prefixes/default/drive_c");
        assert_eq!(
            std::fs::read(drive_c.join("game/Game.exe")).unwrap_or_default(),
            b"MZ"
        );
        assert!(drive_c.join("game/data/level.bin").is_file());
        assert!(outcome.candidates.is_empty(), "no menu areas yet");
        assert!(
            service.list()?.is_empty(),
            "an archive registers nothing yet"
        );
        // Path-traversal is refused end-to-end, naming the entry.
        let evil = root.join("evil.zip");
        build_zip(&evil, &[("../evil.exe", "MZ")]);
        let err = service
            .install(&evil, "default", None, AppKind::Game, ArtifactKind::Archive)
            .expect_err("traversal must be refused");
        assert!(
            err.to_string().contains("escape the prefix"),
            "the refusal is named: {err}"
        );
        assert!(!root.join("prefixes").join("evil.exe").exists());
        Ok(())
    }

    #[test]
    fn artifact_hint_uses_the_filename_default() {
        // Blueprint §8: the branch has a filename-hint default — never
        // silently fixed. `.zip` hints archive, installer-ish names hint
        // installer, everything else standalone.
        assert_eq!(
            artifact_hint(Path::new("/tmp/setup.exe")),
            ArtifactKind::Installer
        );
        assert_eq!(
            artifact_hint(Path::new("/downloads/GameInstaller-2.0.exe")),
            ArtifactKind::Installer
        );
        assert_eq!(
            artifact_hint(Path::new("/games/bundle.zip")),
            ArtifactKind::Archive
        );
        assert_eq!(
            artifact_hint(Path::new("/games/balatro.exe")),
            ArtifactKind::Standalone
        );
        assert_eq!(
            artifact_hint(Path::new("/games/BALATRO.EXE")),
            ArtifactKind::Standalone,
            "the hint matches case-insensitively"
        );
    }

    #[test]
    fn parse_keep_indices_validates_numbers() {
        assert_eq!(parse_keep_indices("1,3", 5).unwrap(), [0, 2]);
        assert_eq!(parse_keep_indices("1 3", 5).unwrap(), [0, 2]);
        assert_eq!(parse_keep_indices("", 5).unwrap(), Vec::<usize>::new());
        assert!(parse_keep_indices("0", 5).is_err(), "1-based numbering");
        assert!(parse_keep_indices("6", 5).is_err(), "out of range");
        assert!(parse_keep_indices("x", 5).is_err(), "not a number");
    }

    #[test]
    fn parse_artifact_choice_accepts_numbers_and_words() {
        let default = ArtifactKind::Standalone;
        assert_eq!(parse_artifact_choice("", default), Some(default));
        assert_eq!(
            parse_artifact_choice("1", default),
            Some(ArtifactKind::Installer)
        );
        assert_eq!(
            parse_artifact_choice("i", default),
            Some(ArtifactKind::Installer)
        );
        assert_eq!(
            parse_artifact_choice("  Installer ", default),
            Some(ArtifactKind::Installer)
        );
        assert_eq!(
            parse_artifact_choice("2", default),
            Some(ArtifactKind::Archive)
        );
        assert_eq!(
            parse_artifact_choice("a", default),
            Some(ArtifactKind::Archive)
        );
        assert_eq!(parse_artifact_choice("3", default), Some(default));
        assert_eq!(parse_artifact_choice("q", default), None);
    }

    fn pick_prefix(slug: &str) -> Prefix {
        Prefix {
            slug: slug.to_owned(),
            defaults: cellar_core::PrefixDefaults::default(),
        }
    }

    #[test]
    fn parse_prefix_choice_picks_numbers_new_names_and_the_default() {
        // Blueprint §8: the empty line is the `default` default; a number
        // picks a listed prefix; anything else names a new one.
        assert_eq!(
            parse_prefix_choice("", 2).unwrap(),
            PrefixPick::New("default".to_owned()),
            "the empty line is the default"
        );
        assert_eq!(
            parse_prefix_choice("2", 2).unwrap(),
            PrefixPick::Existing(1),
            "the numbers are 1-based over the listing"
        );
        assert_eq!(
            parse_prefix_choice("my games", 2).unwrap(),
            PrefixPick::New("my games".to_owned())
        );
        assert_eq!(
            parse_prefix_choice("  0 ", 2).unwrap_err(),
            "prefix 0 is out of range (the list showed 1..=2)"
        );
        assert_eq!(
            parse_prefix_choice("3", 2).unwrap_err(),
            "prefix 3 is out of range (the list showed 1..=2)"
        );
        assert_eq!(
            parse_prefix_choice("1", 0).unwrap_err(),
            "prefix 1 is out of range (the list showed 1..=0)"
        );
    }

    #[test]
    fn resolve_prefix_pick_reuses_existing_and_slugifies_new_names() {
        let prefixes = [pick_prefix("default"), pick_prefix("my-games")];
        // A number resolves to the listed prefix's slug.
        assert_eq!(
            resolve_prefix_pick(PrefixPick::Existing(1), &prefixes).unwrap(),
            "my-games"
        );
        // A name that slugifies to an existing prefix reuses it — typing
        // "My Games" picks `my-games`, it never creates a `-2` sibling.
        assert_eq!(
            resolve_prefix_pick(PrefixPick::New("My Games".to_owned()), &prefixes).unwrap(),
            "my-games"
        );
        assert_eq!(
            resolve_prefix_pick(PrefixPick::New("default".to_owned()), &prefixes).unwrap(),
            "default",
            "the default name reuses the existing default prefix"
        );
        // A genuinely new name resolves to its slug; the session creates it.
        assert_eq!(
            resolve_prefix_pick(PrefixPick::New("New Games".to_owned()), &prefixes).unwrap(),
            "new-games"
        );
        // An unslugifiable name is refused before the session sees it.
        assert_eq!(
            resolve_prefix_pick(PrefixPick::New("!!!".to_owned()), &prefixes).unwrap_err(),
            "cannot form a prefix slug from \"!!!\""
        );
        assert!(
            resolve_prefix_pick(PrefixPick::Existing(9), &prefixes).is_err(),
            "an out-of-range index is refused, never a panic"
        );
    }

    #[test]
    fn resolve_prefix_slug_flag_prompt_and_default_are_one_decision() -> anyhow::Result<()> {
        // Blueprint §8: the flag, the prompt, and the default resolve the
        // same way; the prompt only when the session is interactive (stdin
        // is a TTY, no `--no-input`); the `default` default otherwise —
        // the popup's flagless invocation (#32) is exactly the third arm.
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-prefix-resolve-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(root.clone());
        std::fs::create_dir_all(&root)?;
        PrefixService::new(store.clone()).create("My Games")?;
        assert_eq!(
            resolve_prefix_slug(Some("games"), true, &store)?,
            "games",
            "a valid slug passes through — the flag decides, no prompt"
        );
        assert_eq!(
            resolve_prefix_slug(Some("games"), false, &store)?,
            "games",
            "the flag also decides the non-interactive path"
        );
        assert_eq!(
            resolve_prefix_slug(None, false, &store)?,
            "default",
            "flagless and non-interactive is the default"
        );
        // The flag is the prompt's equivalent: the same name resolution —
        // slugified, reusing the existing prefix the slug names.
        assert_eq!(
            resolve_prefix_slug(Some("My Games"), false, &store)?,
            "my-games",
            "a human name through the flag reuses the existing prefix"
        );
        assert_eq!(
            resolve_prefix_slug(Some("New Games"), false, &store)?,
            "new-games",
            "a new name through the flag is the slug the session creates"
        );
        assert!(
            resolve_prefix_slug(Some("!!!"), false, &store).is_err(),
            "an unslugifiable flag is an operation error, never a guess"
        );
        Ok(())
    }

    /// The real desktop adapter over a test tree: real entry files land in
    /// an `applications/` directory beside a per-test sub-root, icons in
    /// its disposable cache — the composition-root wiring, with every
    /// test's tree a `data-home/cellar`-shaped pair so the sweeps of
    /// parallel tests never share a directory. The `desktop-test`
    /// sub-root keeps the adapter's layout math under the test's own
    /// root even for tests whose tree root is a bare tempdir.
    fn test_desktop(store: &TreeStore) -> DesktopService {
        DesktopService::new(
            store.data_root().join("desktop-test"),
            PathBuf::from("/bin/false"),
        )
    }

    #[test]
    fn run_install_defaults_the_prefix_flaglessly_without_a_tty() -> anyhow::Result<()> {
        // The no-TTY, no-flag session is the popup invocation (#32): the
        // prefix defaults to `default` (blueprint §8) — the flag, prompt,
        // and default are the same decision made at the right layer.
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-popup-default-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(root.clone());
        std::fs::create_dir_all(&root)?;
        let exe = root.join("balatro.exe");
        std::fs::write(&exe, "MZ")?;
        let code = run_install(
            &store,
            &test_desktop(&store),
            &InstallArgs {
                path: exe,
                prefix: None,
                name: None,
                kind: AppKind::Game,
                artifact: None,
                no_input: true,
                keep: Vec::new(),
                keep_all: false,
                add: Vec::new(),
            },
        )?;
        assert_eq!(code, ExitCode::SUCCESS);
        let apps = store.list_apps()?;
        assert_eq!(apps.len(), 1, "the session registered the exe");
        assert_eq!(apps[0].prefix, "default", "the flagless default prefix");
        Ok(())
    }

    #[test]
    fn install_writes_the_launcher_entry_and_uninstall_removes_it() -> anyhow::Result<()> {
        // AC (#33): registering an app creates its launcher entry;
        // uninstalling removes it — through the real adapter over the
        // test tree, exactly as the composition root injects it. The
        // home holds both the tree and, beside it, the applications
        // directory the system reads.
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-entry-lifecycle-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(home.join("cellar"));
        std::fs::create_dir_all(home.join("cellar"))?;
        let exe = home.join("cellar/balatro.exe");
        std::fs::write(&exe, "MZ")?;
        let desktop = test_desktop(&store);
        run_install(
            &store,
            &desktop,
            &InstallArgs {
                path: exe.clone(),
                prefix: None,
                name: None,
                kind: AppKind::Game,
                artifact: Some(ArtifactKind::Standalone),
                no_input: true,
                keep: Vec::new(),
                keep_all: false,
                add: Vec::new(),
            },
        )?;
        let entry = home.join("cellar/applications/cellar-balatro.desktop");
        assert!(
            entry.exists(),
            "registering an app creates its launcher entry"
        );
        let rendered = std::fs::read_to_string(&entry)?;
        assert!(
            rendered.contains("launch balatro\n"),
            "the entry launches the app"
        );
        let service = InstallService::new(
            store.clone(),
            ResolverSet::new(all_resolvers(&store.data_root().join("runtime"))),
            desktop,
        );
        service.uninstall("balatro")?;
        assert!(!entry.exists(), "uninstalling removes the launcher entry");
        Ok(())
    }

    #[test]
    fn reinstalling_with_a_new_name_renames_the_launcher_entry() -> anyhow::Result<()> {
        // AC (#33): renaming an app updates the entry file name — the
        // identity stays the exe path, so re-install with a new name
        // moves the `.desktop` file.
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-entry-rename-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(home.join("cellar"));
        std::fs::create_dir_all(home.join("cellar"))?;
        let exe = home.join("cellar/balatro.exe");
        std::fs::write(&exe, "MZ")?;
        let desktop = test_desktop(&store);
        let args = |name: Option<&str>| InstallArgs {
            path: exe.clone(),
            prefix: None,
            name: name.map(str::to_owned),
            kind: AppKind::Game,
            artifact: Some(ArtifactKind::Standalone),
            no_input: true,
            keep: Vec::new(),
            keep_all: false,
            add: Vec::new(),
        };
        run_install(&store, &desktop, &args(None))?;
        let before = home.join("cellar/applications/cellar-balatro.desktop");
        assert!(before.exists(), "the first entry exists");
        run_install(&store, &desktop, &args(Some("Poker Night")))?;
        assert!(!before.exists(), "the old entry file name is gone");
        let after = home.join("cellar/applications/cellar-poker-night.desktop");
        assert!(after.exists(), "the entry file name follows the rename");
        Ok(())
    }

    #[test]
    fn desktop_sync_re_derives_entries_and_prunes_stale() -> anyhow::Result<()> {
        // AC (#33): deleting the cache leaves entries functional — the
        // re-derivation restores entries, icons, and the association
        // from the tree, and prunes what a rename or removal left behind.
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-desktop-sync-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(home.join("cellar"));
        std::fs::create_dir_all(home.join("cellar"))?;
        let exe = home.join("cellar/balatro.exe");
        std::fs::write(&exe, "MZ")?;
        let desktop = test_desktop(&store);
        let args = |path: &Path| InstallArgs {
            path: path.to_path_buf(),
            prefix: None,
            name: None,
            kind: AppKind::Game,
            artifact: Some(ArtifactKind::Standalone),
            no_input: true,
            keep: Vec::new(),
            keep_all: false,
            add: Vec::new(),
        };
        let tool = home.join("cellar/helper.exe");
        std::fs::write(&tool, "MZ")?;
        run_install(&store, &desktop, &args(&exe))?;
        run_install(&store, &desktop, &args(&tool))?;
        let applications = home.join("cellar/applications");
        let balatro_entry = applications.join("cellar-balatro.desktop");
        let tool_entry = applications.join("cellar-helper.desktop");
        assert!(balatro_entry.exists() && tool_entry.exists());
        // The user's wreck: an entry, the icon cache, and one app's exe
        // are gone; another app was renamed by hand — the file AND its
        // slug (the tree contract: file name = entry's display name).
        std::fs::remove_file(&balatro_entry)?;
        std::fs::remove_dir_all(home.join("cellar/desktop-test/cache")).ok();
        std::fs::remove_file(&exe).ok();
        let renamed = std::fs::read_to_string(home.join("cellar/apps/balatro.toml"))?
            .replace("slug = \"balatro\"", "slug = \"poker-night\"");
        std::fs::write(home.join("cellar/apps/poker-night.toml"), renamed)?;
        std::fs::remove_file(home.join("cellar/apps/balatro.toml"))?;
        // One re-derivation restores everything derived and removes the
        // stale entry the rename left behind — the missing exe's entry
        // still re-derives (only its icon cannot).
        let code = run_desktop_sync(&store, &desktop)?;
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(applications.join("cellar-poker-night.desktop").exists());
        assert!(tool_entry.exists(), "the untouched app keeps its entry");
        assert!(
            !balatro_entry.exists(),
            "the stale entry under the old slug is pruned"
        );
        assert!(
            applications.join("open-with-cellar.desktop").exists(),
            "the Open-with-Cellar association is wired"
        );
        Ok(())
    }

    #[test]
    fn runner_install_unknown_provider_is_an_operation_error() {
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-runner-bad-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(home.join("cellar"));
        let err = run_runner_install(&store, "cartman", "9.0")
            .expect_err("an unknown provider is an operation error, never a guess");
        assert!(
            err.to_string().contains("proton") && err.to_string().contains("umu"),
            "the error names the managed providers: {err}"
        );
    }

    #[test]
    fn runner_list_renders_managed_rows_and_json() -> anyhow::Result<()> {
        // The inventory is the authoritative record (blueprint §6): a
        // hand-written `providers.toml` renders as the managed rows of
        // `runner list`. The discover-only half depends on host state
        // (PATH, Steam dirs) — covered at the provider level; here the
        // inventory + both output shapes are pinned.
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-runner-list-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(home.join("cellar"));
        std::fs::create_dir_all(home.join("cellar/runtime"))?;
        std::fs::write(
            home.join("cellar/runtime/providers.toml"),
            "schema_version = 1\n\n[[runner]]\nprovider_id = \"proton\"\n\
             version = \"GE-Proton11-5\"\ninstall = \"proton/GE-Proton11-5\"\n\
             [[runner]]\nprovider_id = \"umu\"\n\
             version = \"1.4.4\"\ninstall = \"umu/1.4.4\"\n",
        )?;
        assert_eq!(run_runner_list(&store, false)?, ExitCode::SUCCESS);
        assert_eq!(run_runner_list(&store, true)?, ExitCode::SUCCESS);
        Ok(())
    }

    #[test]
    fn dry_run_for_a_managed_proton_app_shows_the_umu_stack() -> anyhow::Result<()> {
        // AC: the dry-run plan for a managed Proton app shows the
        // canonical stack — `umu-run → (SLR, internal) → proton
        // waitforexitandrun → exe` — as the delegation (research #18)
        // with the umu env contract contributed by the wrapper, riding
        // the real registry over the test tree.
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-managed-plan-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(home.join("cellar"));
        std::fs::create_dir_all(home.join("cellar"))?;
        // Managed installs exactly as the pipeline lays them out (probe
        // targets included — resolution scans dirs, not the inventory).
        write_executable(&home.join("cellar/runtime/proton/GE-Proton11-5/proton"))?;
        write_executable(&home.join("cellar/runtime/umu/1.4.4/umu-run"))?;
        let exe = home.join("cellar/game.exe");
        std::fs::write(&exe, "MZ")?;
        let desktop = test_desktop(&store);
        run_install(
            &store,
            &desktop,
            &InstallArgs {
                path: exe.clone(),
                prefix: None,
                name: None,
                kind: AppKind::Game,
                artifact: Some(ArtifactKind::Standalone),
                no_input: true,
                keep: Vec::new(),
                keep_all: false,
                add: Vec::new(),
            },
        )?;
        let app = LaunchApp::with_chain(store.clone(), resolvers_for(&store), wrappers_for);
        let plan = app.plan("game", &[])?;
        assert_eq!(
            plan.argv.first(),
            Some(
                &home
                    .join("cellar/runtime/umu/1.4.4/umu-run")
                    .to_string_lossy()
                    .into_owned()
            ),
            "umu-run is the outermost of the spawn — the delegation"
        );
        assert_eq!(plan.argv.get(1), Some(&exe.to_string_lossy().into_owned()));
        assert_eq!(plan.env.get("GAMEID").map(String::as_str), Some("umu-game"));
        assert_eq!(
            plan.env.get("WINEPREFIX").map(String::as_str),
            store.prefix_dir("default").to_str()
        );
        assert_eq!(
            plan.env.get("PROTONPATH").map(String::as_str),
            Some(
                home.join("cellar/runtime/proton/GE-Proton11-5")
                    .to_str()
                    .unwrap()
            )
        );
        assert_eq!(
            plan.env.get("PROTON_VERB").map(String::as_str),
            Some("waitforexitandrun")
        );
        assert_eq!(plan.wrappers, [cellar_core::Layer::Container]);
        Ok(())
    }

    #[test]
    fn gamescope_joins_the_display_layer_when_the_prefix_configures_it() -> anyhow::Result<()> {
        // AC: gamescope contributes at the Display layer when configured
        // (the prefix's `graphics = "gamescope"` default) — outermost of
        // the same managed-Proton chain.
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!(
            "cellar-cli-e2e-gamescope-{}-{seq}",
            std::process::id()
        ));
        let store = TreeStore::new(home.join("cellar"));
        std::fs::create_dir_all(home.join("cellar"))?;
        write_executable(&home.join("cellar/runtime/proton/GE-Proton11-5/proton"))?;
        write_executable(&home.join("cellar/runtime/umu/1.4.4/umu-run"))?;
        let exe = home.join("cellar/game.exe");
        std::fs::write(&exe, "MZ")?;
        let desktop = test_desktop(&store);
        run_install(
            &store,
            &desktop,
            &InstallArgs {
                path: exe.clone(),
                prefix: None,
                name: None,
                kind: AppKind::Game,
                artifact: Some(ArtifactKind::Standalone),
                no_input: true,
                keep: Vec::new(),
                keep_all: false,
                add: Vec::new(),
            },
        )?;
        let mut prefix = store.load_prefix("default")?;
        prefix.defaults.graphics = Some("gamescope".to_owned());
        store.save_prefix(&prefix)?;
        let app = LaunchApp::with_chain(store.clone(), resolvers_for(&store), wrappers_for);
        let plan = app.plan("game", &[])?;
        assert_eq!(
            plan.wrappers,
            [cellar_core::Layer::Display, cellar_core::Layer::Container]
        );
        assert_eq!(plan.argv.first().map(String::as_str), Some("gamescope"));
        assert!(plan.argv[1].ends_with("umu-run"));
        Ok(())
    }

    fn write_executable(path: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, "#!/bin/sh\nexit 0\n")?;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
        Ok(())
    }

    fn review_candidate(label: &str) -> Candidate {
        Candidate {
            exe: PathBuf::from(format!("/prefix/drive_c/{label}.exe")),
            label: label.to_owned(),
        }
    }

    #[test]
    fn select_kept_applies_keep_all_and_validates_indices() -> anyhow::Result<()> {
        let candidates = ["a", "b", "c"].map(review_candidate);
        let args = || InstallArgs {
            path: PathBuf::from("/tmp/setup.exe"),
            prefix: None,
            name: None,
            kind: AppKind::Game,
            artifact: Some(ArtifactKind::Installer),
            no_input: true,
            keep: Vec::new(),
            keep_all: false,
            add: Vec::new(),
        };
        assert_eq!(
            select_kept(&candidates, &args())?.len(),
            0,
            "no flags keep nothing — never silent registration"
        );
        let mut subset = args();
        subset.keep = vec![2, 3];
        let kept = select_kept(&candidates, &subset)?;
        assert_eq!(
            kept.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            ["b", "c"],
            "--keep takes the printed numbers"
        );
        let mut all = args();
        all.keep_all = true;
        assert_eq!(
            select_kept(&candidates, &all)?.len(),
            3,
            "--keep-all keeps every one"
        );
        let mut bad = args();
        bad.keep = vec![4];
        assert!(
            select_kept(&candidates, &bad).is_err(),
            "an out-of-range number is an operation error"
        );
        Ok(())
    }

    #[test]
    fn registration_summary_lists_entries_and_next_commands() {
        let result = |slug: &str, kind: AppKind, update: bool| InstallResult {
            entry: AppEntry {
                slug: slug.to_owned(),
                exe: PathBuf::from(format!("/prefix/drive_c/{slug}.exe")),
                kind,
                prefix: "default".to_owned(),
                overrides: Overrides::default(),
                runner: None,
                source_installer: Some(PathBuf::from("/tmp/setup.exe")),
                installed_at: None,
            },
            was_update: update,
        };
        let summary = registration_summary(
            &[
                result("game-a", AppKind::Game, false),
                result("game-b", AppKind::Game, true),
            ],
            "default",
        );
        assert!(
            summary.contains("Registered 1 entry in prefix 'default'"),
            "created vs updated are told apart:\n{summary}"
        );
        assert!(
            summary.contains("Updated 1 existing entry"),
            "update line:\n{summary}"
        );
        assert!(
            summary.contains("cellar launch game-a") && summary.contains("cellar launch game-b"),
            "the next command per entry:\n{summary}"
        );
        let empty = registration_summary(&[], "default");
        assert!(
            empty.contains("Registered nothing") && empty.contains("--keep"),
            "an empty review is a plain outcome, not an error:\n{empty}"
        );
    }

    /// The installer e2e rig: a stub wine that plants `names` on the
    /// Desktop and reports success, plus the installer artifact and a
    /// configured prefix — the full installer → awaited exit → discovery
    /// loop in one test.
    #[cfg(unix)]
    fn installer_rig(tag: &str, names: &[&str]) -> anyhow::Result<(TreeStore, PathBuf)> {
        use cellar_core::ConfiguredRunner;

        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("cellar-cli-e2e-{tag}-{}-{seq}", std::process::id()));
        let store = TreeStore::new(root.clone());
        std::fs::create_dir_all(&root)?;
        let wine = root.join("stub-wine");
        let plants = names
            .iter()
            .map(|name| format!("echo MZ > \"$WINEPREFIX/drive_c/users/me/Desktop/{name}.exe\""))
            .collect::<Vec<_>>()
            .join("\n");
        write_stub_script(
            &wine,
            &format!("mkdir -p \"$WINEPREFIX/drive_c/users/me/Desktop\"\n{plants}\nexit 0\n"),
        )?;
        let installer = root.join("setup.exe");
        std::fs::write(&installer, "MZ-setup")?;
        PrefixService::new(store.clone()).create("default")?;
        let mut prefix = store.load_prefix("default")?;
        prefix.defaults.runner = Some(RunnerSpec::with_configured(
            RunnerFamily::Wine,
            ConfiguredRunner::Path(wine),
        ));
        store.save_prefix(&prefix)?;
        Ok((store, installer))
    }

    #[test]
    #[cfg(unix)]
    fn installer_dropping_five_exes_registers_up_to_five_entries_with_keep_all()
    -> anyhow::Result<()> {
        // Acceptance: an installer dropping five exes yields up to five
        // entries — no guessing a main one, no silent registration. Here
        // `--keep-all` is the review's decision; the session stays one
        // prefix.
        let (store, installer) = installer_rig(
            "review-all",
            &["game", "launcher", "tool", "helper", "analyzer"],
        )?;
        let code = run_install(
            &store,
            &test_desktop(&store),
            &InstallArgs {
                path: installer,
                prefix: None,
                name: None,
                kind: AppKind::Game,
                artifact: Some(ArtifactKind::Installer),
                no_input: true,
                keep: Vec::new(),
                keep_all: true,
                add: Vec::new(),
            },
        )?;
        assert_eq!(code, ExitCode::SUCCESS);
        let apps = store.list_apps()?;
        let slugs: Vec<&str> = apps.iter().map(|app| app.slug.as_str()).collect();
        assert_eq!(
            slugs,
            ["analyzer", "game", "helper", "launcher", "tool"],
            "five candidates, five entries"
        );
        assert!(
            apps.iter().all(|app| app.prefix == "default"),
            "one session, one prefix"
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn installer_review_keep_subset_registers_only_the_confirmed() -> anyhow::Result<()> {
        // `--keep 1 3 5` keeps exactly the printed numbers; the rest stay
        // hidden.
        let (store, installer) = installer_rig(
            "review-subset",
            &["game", "launcher", "tool", "helper", "analyzer"],
        )?;
        run_install(
            &store,
            &test_desktop(&store),
            &InstallArgs {
                path: installer,
                prefix: None,
                name: None,
                kind: AppKind::Game,
                artifact: Some(ArtifactKind::Installer),
                no_input: true,
                keep: vec![1, 3, 5],
                keep_all: false,
                add: Vec::new(),
            },
        )?;
        let slugs: Vec<String> = store.list_apps()?.into_iter().map(|app| app.slug).collect();
        assert_eq!(
            slugs,
            ["analyzer", "helper", "tool"],
            "only the confirmed candidates register"
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn installer_review_without_confirmation_registers_nothing() -> anyhow::Result<()> {
        // The hard rule enforced end-to-end: candidates alone never
        // register — no flags, no prompts, zero entries, success exit.
        let (store, installer) = installer_rig("review-none", &["game", "tool"])?;
        let code = run_install(
            &store,
            &test_desktop(&store),
            &InstallArgs {
                path: installer,
                prefix: None,
                name: None,
                kind: AppKind::Game,
                artifact: Some(ArtifactKind::Installer),
                no_input: true,
                keep: Vec::new(),
                keep_all: false,
                add: Vec::new(),
            },
        )?;
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(
            store.list_apps()?.is_empty(),
            "nothing registers without confirmation"
        );
        Ok(())
    }
}
