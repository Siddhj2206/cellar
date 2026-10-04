//! The launch flag-ordering contract (#39), at the level CONTRIBUTING.md
//! asks for it: end-to-end CLI behavior — exit code and stderr text —
//! asserted against the real binary.
//!
//! `LaunchArgs::args` accepts hyphen-leading values so `cellar launch game
//! -windowed` needs no separator, but that allowance ends clap's flag scan.
//! `cellar launch game -windowed --dry-run` used to LAUNCH the game and
//! hand it `--dry-run`: the user asked for a plan preview and got the
//! opposite, silently. Cellar now refuses and names both orderings. These
//! tests pin the parts a unit test cannot: the exit code is 2 (usage, ADR
//! 0004) rather than 1, and nothing is spawned.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The presentation binary under test.
fn cellar() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cellar"))
}

/// A scratch data home: `$XDG_DATA_HOME` pins where the child builds its
/// tree (ADR 0001) without touching the developer's real one.
fn xdg_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cellar-order-{tag}-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).expect("scratch data home");
    dir
}

fn launch(xdg: &Path, args: &[&str]) -> Output {
    cellar()
        .arg("launch")
        .args(args)
        .stdin(Stdio::null())
        .env("XDG_DATA_HOME", xdg)
        .output()
        .unwrap_or_else(|e| panic!("command failed to spawn: {e}"))
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Register a real entry so the refused invocations get past argument
/// handling — the guard must be what stops them, not a missing app.
fn register(xdg: &Path, tag: &str) -> PathBuf {
    let exe = xdg.join(format!("{tag}.exe"));
    fs::write(&exe, b"MZ stub").expect("write exe");
    let output = cellar()
        .args(["install"])
        .arg(&exe)
        .args(["--artifact", "standalone", "--no-input"])
        .stdin(Stdio::null())
        .env("XDG_DATA_HOME", xdg)
        .output()
        .expect("install spawns");
    assert_eq!(
        output.status.code(),
        Some(0),
        "the fixture entry registers: {}",
        stderr_of(&output)
    );
    exe
}

#[test]
fn a_cellar_flag_after_an_app_argument_exits_2_with_both_orderings() {
    let xdg = xdg_home("refuse");
    register(&xdg, "warpinator");
    for flag in ["--dry-run", "--detach", "--json", "--quiet"] {
        let output = launch(&xdg, &["warpinator", "-windowed", flag]);
        let stderr = stderr_of(&output);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{flag} after an app arg is a usage error (ADR 0004), got {:?}: {stderr}",
            output.status.code()
        );
        assert!(
            stderr.contains(&format!("'{flag}'")),
            "the message names the swallowed flag: {stderr}"
        );
        assert!(
            stderr.contains("cellar launch warpinator --dry-run")
                || stderr.contains(&format!("cellar launch warpinator {flag}")),
            "…and the ordering that works: {stderr}"
        );
        assert!(
            stderr.contains("--"),
            "…including the `--` separator as the other option: {stderr}"
        );
        assert!(
            output.stdout.is_empty(),
            "a refused launch prints no plan and spawns nothing: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn the_flag_first_ordering_still_plans() {
    // The refusal must not cost the working ordering: `cellar launch game
    // --dry-run -windowed` is a plan preview, exit 0.
    let xdg = xdg_home("flags-first");
    register(&xdg, "warpinator");
    let output = launch(&xdg, &["warpinator", "--dry-run", "-windowed"]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = stderr_of(&output);
    // The fixture has no runner, so the plan itself may fail — what
    // matters is that the failure is a *plan* failure (the pipeline ran) and
    // not the guard's ordering refusal.
    assert!(
        !stderr.contains("is taken as an argument for the app"),
        "the working ordering must not trip the guard: {stderr}"
    );
    assert!(
        stdout.contains("warpinator") || stderr.contains("runner could be resolved"),
        "the launch pipeline ran and failed on the missing runner, as a fixture \
         with no runner should: {stdout}{stderr}"
    );
}

#[test]
fn a_separator_hands_the_flag_to_the_app() {
    // The escape hatch the refusal recommends has to work: with `--`,
    // `--json` is the game's argument, so the guard must stay silent and
    // the launch proceed (here failing later, on the missing runner — not
    // on ordering).
    let xdg = xdg_home("separator");
    register(&xdg, "warpinator");
    let output = launch(&xdg, &["warpinator", "--", "-windowed", "--json"]);
    let stderr = stderr_of(&output);
    assert!(
        !stderr.contains("is taken as an argument for the app"),
        "`--` means the user meant it for the app: {stderr}"
    );
}

#[test]
fn ordinary_game_arguments_are_never_refused() {
    // The narrowness: a token that is not one of Cellar's flags is the
    // game's, including ones whose letters contain a Cellar short.
    let xdg = xdg_home("game-args");
    register(&xdg, "warpinator");
    for args in [
        vec!["warpinator", "-windowed", "--fullscreen", "1920x1080"],
        vec!["warpinator", "--", "-windowed", "--json"],
        vec!["warpinator", "--", "-dx11"],
    ] {
        let output = launch(&xdg, &args);
        let stderr = stderr_of(&output);
        assert_ne!(
            output.status.code(),
            Some(2),
            "{args:?} are the game's arguments: {stderr}"
        );
        assert!(
            !stderr.contains("is taken as an argument for the app"),
            "{args:?} must not read as a swallowed Cellar flag: {stderr}"
        );
    }
}
