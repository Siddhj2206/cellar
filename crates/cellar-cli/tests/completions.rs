//! The `cellar completions <shell>` contract (#49, ADR 0004): a static
//! completion script generated from the same clap definition the binary
//! parses with, printed to stdout, installable by redirection.
//!
//! These are black-box tests against the real binary because the parts worth
//! pinning are only visible from outside: the script's bytes name the real
//! commands (so a rename breaks this test rather than shipping a script that
//! completes a command that no longer exists), an unknown shell is a *usage*
//! error with exit 2 and clap's possible-values list, and the command needs
//! no data root at all — printing a script must not create state.
//!
//! Nothing here depends on the host's shells: the script is text, and its
//! shape is asserted, not executed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The presentation binary under test.
fn cellar() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cellar"))
}

/// A scratch data home: `$XDG_DATA_HOME` pins where the child would build
/// its tree (ADR 0001) without touching the developer's real one. The
/// completions command must never need it — a test asserts that.
fn xdg_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cellar-completions-{tag}-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    dir
}

fn spawn(args: &[&str], xdg: &Path) -> Output {
    cellar()
        .args(args)
        .stdin(Stdio::null())
        .env("XDG_DATA_HOME", xdg)
        .output()
        .unwrap_or_else(|e| panic!("command failed to spawn: {e}"))
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The shells Cellar advertises on the command line. `powershell` comes
/// free from `clap_complete::Shell`'s own vocabulary; it is listed here so
/// a future narrowing of the argument is a deliberate, visible change.
const SHELLS: [&str; 5] = ["bash", "elvish", "fish", "powershell", "zsh"];

/// Every command name the contract puts at the surface (ADR 0004) — the
/// noun groups included. Each must appear in every generated script, so a
/// renamed command or group fails here instead of shipping a script that
/// completes a name the binary no longer accepts.
const COMMANDS: [&str; 8] = [
    "install",
    "list",
    "launch",
    "uninstall",
    "prefix",
    "desktop",
    "runner",
    "doctor",
];

#[test]
fn every_shell_emits_a_non_empty_script() {
    let xdg = xdg_home("emit");
    for shell in SHELLS {
        let output = spawn(&["completions", shell], &xdg);
        let script = stdout_of(&output);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{shell}: the script is the command's product, exit 0: {}",
            stderr_of(&output)
        );
        assert!(
            script.len() > 500,
            "{shell}: a completion script is not a line or two, got {} bytes",
            script.len()
        );
        assert!(
            script.contains("cellar"),
            "{shell}: the script registers itself for the binary it completes"
        );
        assert!(
            stderr_of(&output).is_empty(),
            "{shell}: stdout is the only stream the command writes: {}",
            stderr_of(&output)
        );
    }
}

#[test]
fn the_script_names_the_real_command_surface() {
    let xdg = xdg_home("surface");
    for shell in SHELLS {
        let script = stdout_of(&spawn(&["completions", shell], &xdg));
        for command in COMMANDS {
            assert!(
                script.contains(command),
                "{shell}: the script completes `{command}` — a command renamed away \
                 without updating the surface must fail here"
            );
        }
        // The subcommand set: flags, the global quiet, and the noun groups'
        // own verbs. fish spells long options `-l name` rather than
        // `--name`; both are the same flag, so the spelling follows the
        // shell.
        let long = |flag: &str| {
            if shell == "fish" {
                format!("-l {}", flag.trim_start_matches("--"))
            } else {
                flag.to_owned()
            }
        };
        for flag in ["--quiet", "--json", "--dry-run", "--no-input", "--keep-all"] {
            assert!(
                script.contains(&long(flag)),
                "{shell}: the script completes `{flag}`"
            );
        }
        for verb in ["sync", "create", "delete"] {
            assert!(
                script.contains(verb),
                "{shell}: the script completes the group verb `{verb}`"
            );
        }
    }
}

#[test]
fn enumerated_flag_values_complete_from_their_own_vocabulary() {
    // The value completions that cost nothing with a static generator: the
    // allowed values come from the same `ALL` lists the parsers accept
    // (`AppKind::ALL`, `ArtifactKind::ALL`), so there is no second list to
    // drift. fish spells them out one per line, which makes the assertion
    // exact.
    let xdg = xdg_home("values");
    let script = stdout_of(&spawn(&["completions", "fish"], &xdg));
    for value in ["standalone", "installer", "archive", "game", "tool"] {
        assert!(
            script.contains(value),
            "`--artifact`/`--kind` complete `{value}`: the vocabulary the parser \
             accepts is the one the script offers"
        );
    }
    // The shell argument completes its own values too — the generated
    // script can therefore complete the command that generated it. bash and
    // zsh are the two generators that spell a positional's possible values
    // out; fish and the rest leave the positional to the shell's default, so
    // the assertion is not made about their output.
    for shell in ["bash", "zsh"] {
        let script = stdout_of(&spawn(&["completions", shell], &xdg));
        for value in SHELLS {
            assert!(
                script.contains(value),
                "{shell}: `cellar completions <TAB>` completes `{value}`"
            );
        }
    }
}

#[test]
fn an_unknown_shell_is_a_usage_error_naming_the_ones_that_work() {
    let xdg = xdg_home("unknown");
    let output = spawn(&["completions", "nope"], &xdg);
    let stderr = stderr_of(&output);
    assert_eq!(
        output.status.code(),
        Some(2),
        "an unknown shell is usage, not an operation error (ADR 0004): {stderr}"
    );
    assert!(
        stderr.contains("nope"),
        "the message names what was typed: {stderr}"
    );
    for shell in SHELLS {
        assert!(
            stderr.contains(shell),
            "the message lists the shells that do work, `{shell}` missing: {stderr}"
        );
    }
    assert!(
        stdout_of(&output).is_empty(),
        "a rejected shell prints no partial script: {}",
        stdout_of(&output)
    );
    // A near miss gets clap's suggestion machinery, like every other
    // unknown value on the surface.
    let output = spawn(&["completions", "bosh"], &xdg);
    assert_eq!(output.status.code(), Some(2), "still usage");
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("bash"),
        "a near miss names the intended shell: {stderr}"
    );
}

#[test]
fn help_lists_the_subcommand_and_shows_the_install_line() {
    let xdg = xdg_home("help");
    let help = stdout_of(&spawn(&["--help"], &xdg));
    assert!(
        help.contains("completions"),
        "the root help lists the subcommand — a user who does not know the \
         completion line exists finds it there"
    );
    let help = stdout_of(&spawn(&["completions", "--help"], &xdg));
    assert_eq!(
        spawn(&["completions", "--help"], &xdg).status.code(),
        Some(0),
        "--help exits 0"
    );
    // Examples-first (ADR 0004): the help leads with the examples, and they
    // are the redirection lines that install the script.
    let examples = help
        .split_once("Examples:")
        .expect("the help template's examples block")
        .1;
    let usage = examples
        .split_once("Usage:")
        .expect("usage follows the examples")
        .0;
    for shell in ["bash", "zsh", "fish"] {
        assert!(
            usage.contains(&format!("cellar completions {shell} >")),
            "the examples show the install line for {shell}:\n{usage}"
        );
    }
    assert!(
        examples.contains("--artifact") && examples.contains("apps/"),
        "the help states what does and does not complete:\n{examples}"
    );
}

#[test]
fn a_bare_invocation_prints_help_and_exits_two() {
    let xdg = xdg_home("bare");
    let output = spawn(&["completions"], &xdg);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a bare required-arg command is usage (ADR 0004)"
    );
    let help = stdout_of(&output);
    assert!(
        help.contains("Examples:") && help.contains("cellar completions bash >"),
        "a bare invocation shows the examples-first help, not clap's condensed \
         usage:\n{help}"
    );
}

#[test]
fn quiet_and_pipes_change_nothing_about_the_product() {
    // The script is delivered data: `--quiet` silences narration, and there
    // is none to silence here. `NO_COLOR` is likewise a non-event — a
    // completion script is plain text by nature, and the color contract
    // forbids ANSI in a pipe anyway.
    let xdg = xdg_home("quiet");
    let plain = stdout_of(&spawn(&["completions", "bash"], &xdg));
    let quiet = stdout_of(&spawn(&["completions", "-q", "bash"], &xdg));
    assert_eq!(plain, quiet, "`--quiet` does not alter the script");
    let no_color = cellar()
        .args(["completions", "bash"])
        .env("NO_COLOR", "1")
        .env("XDG_DATA_HOME", &xdg)
        .stdin(Stdio::null())
        .output()
        .expect("spawns");
    assert_eq!(
        plain,
        stdout_of(&no_color),
        "NO_COLOR does not alter the script"
    );
    assert!(
        !plain.contains('\u{1b}'),
        "no ANSI ever reaches the script, terminal or not"
    );
}

#[test]
fn printing_a_script_needs_no_data_root_and_creates_none() {
    // The command is about the CLI, not the tree: it must work before any
    // install has happened, and it must not bring a store into existence to
    // print. Pointed at a data home that does not exist, it still succeeds
    // and still leaves the directory alone.
    let xdg = xdg_home("no-root");
    assert!(!xdg.exists(), "the scratch home starts absent");
    let output = spawn(&["completions", "bash"], &xdg);
    assert_eq!(output.status.code(), Some(0), "no data root needed");
    assert!(stdout_of(&output).len() > 500, "the script still prints");
    assert!(
        !xdg.exists(),
        "printing a script created state: {:?}",
        fs::read_dir(&xdg).map(std::iter::Iterator::count)
    );
}

#[test]
fn a_misconfigured_data_root_does_not_block_the_script() {
    // `XDG_DATA_HOME` must be absolute (#61): every tree-backed command dies
    // with exit 2 on a relative one. `completions` reads nothing, so it is
    // not gated on the tree at all — the check stays where it was.
    let output = cellar()
        .args(["completions", "bash"])
        .env("XDG_DATA_HOME", "relative/data/home")
        .stdin(Stdio::null())
        .output()
        .expect("spawns");
    assert_eq!(
        output.status.code(),
        Some(0),
        "the script needs no data root: {}",
        stderr_of(&output)
    );
    assert!(stdout_of(&output).len() > 500);
}

#[test]
fn a_closed_pipe_is_not_a_failure() {
    // `cellar completions bash | head` is the reader's choice — the same
    // rule the help rendering already keeps. The child must exit 0, not
    // panic on the broken pipe.
    let xdg = xdg_home("pipe");
    let child = cellar()
        .args(["completions", "bash"])
        .env("XDG_DATA_HOME", &xdg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawns");
    // `head -1`-equivalent: read a little, then drop the pipe.
    let mut child = child;
    {
        use std::io::Read as _;
        let mut first = [0_u8; 64];
        let mut stdout = child.stdout.take().expect("piped");
        let _ = stdout.read(&mut first);
    }
    let status = child.wait().expect("child exits");
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read as _;
        let mut buf = String::new();
        let _ = pipe.read_to_string(&mut buf);
        stderr = buf;
    }
    assert!(
        status.success(),
        "a closed pipe exits 0 (no panic on the reader's choice): {stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "nothing panicked on the broken pipe: {stderr}"
    );
}
