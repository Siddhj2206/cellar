//! The "Open with Cellar" popup contract (#32, ADR 0004): the MIME file
//! association calls the presentation binary's install entrypoint directly
//! — `cellar install <path>` — with no separate popup binary and no shell
//! wrapper in the exec line. A file manager's exec line runs with stdin
//! closed, so these tests spawn the real binary with `Stdio::null()` and
//! assert the no-TTY contract: prompts never appear, the filename hint
//! decides the artifact branch (announced, never silent), the prefix
//! defaults to `default`, and nothing registers without confirmation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The presentation binary the exec line runs.
fn cellar() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cellar"))
}

/// A scratch data home for one test: `$XDG_DATA_HOME` pins where the child
/// builds its tree (ADR 0001) without touching the developer's real one.
fn xdg_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cellar-popup-{tag}-{}", std::process::id()));
    // Clean slate for a re-run of the same binary; a fresh dir needs none.
    fs::remove_dir_all(&dir).ok();
    dir
}

/// Run one command under the scratch environment with stdin closed, the
/// way the MIME exec line runs the binary.
fn spawn(command: &mut Command, xdg: &Path) -> Output {
    command
        .stdin(Stdio::null())
        .env("XDG_DATA_HOME", xdg)
        .output()
        .unwrap_or_else(|e| panic!("command failed to spawn: {e}"))
}

/// The popup invocation itself: `cellar install <path>` (plus any flags
/// the association passes), nothing else.
fn popup(xdg: &Path, args: &[&str]) -> Output {
    spawn(cellar().arg("install").args(args), xdg)
}

/// The prompt markers of every prompt this binary can ask — none may
/// appear in a popup invocation's output (checked on both streams; a
/// prompt writes its question to stdout and headers to stderr). Each
/// marker is a verbatim fragment of its prompt in `crates/cellar-cli/src/
/// main.rs`: keep them in sync when a prompt's wording changes — a drifted
/// marker silently weakens these black-box tests.
const PROMPT_MARKERS: [&str; 6] = [
    "No prefixes yet",          // prompt_prefix, no prefixes
    "Use an existing prefix",   // prompt_prefix, with prefixes
    "How should Cellar handle", // prompt_artifact_kind header (stderr)
    "Choice (1-3",              // prompt_artifact_kind question
    "Keep which",               // interactive_review
    "Manually add",             // interactive_review
];

fn assert_no_prompts(stdout: &str, stderr: &str) {
    for marker in PROMPT_MARKERS {
        assert!(
            !stdout.contains(marker) && !stderr.contains(marker),
            "a prompt leaked into the popup invocation: {marker:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

/// The follow-up `cellar list --json` under the same data home — the
/// durable proof of what the popup invocation registered.
fn list_json(xdg: &Path) -> String {
    let output = spawn(cellar().args(["list", "--json"]), xdg);
    assert_eq!(output.status.code(), Some(0), "list --json must succeed");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn popup_invocation_registers_a_standalone_exe_directly() {
    // A file manager's "Open with Cellar" on a bare exe: the exec line ran
    // `cellar install <path>` with no flags and no TTY. The session uses
    // the `default` prefix, announces the filename hint (standalone —
    // never silently fixed), registers the exe, and prints the summary —
    // all without a single prompt.
    let xdg = xdg_home("standalone");
    let exe = xdg.join("games/balatro.exe");
    fs::create_dir_all(exe.parent().expect("games dir")).expect("mkdir");
    fs::write(&exe, "MZ").expect("write exe");

    let output = popup(&xdg, &[exe.to_str().expect("utf-8 path")]);
    assert_eq!(output.status.code(), Some(0), "the popup run succeeds");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_no_prompts(&stdout, &stderr);
    assert!(
        stderr.contains(
            "Treating 'balatro.exe' as standalone (hint from the file name — \
             pass --artifact to override)"
        ),
        "the branch hint is announced, never silent:\n{stderr}"
    );
    assert!(
        stdout.contains("Registered 'balatro' (game) in prefix 'default'"),
        "the summary names the entry and prefix:\n{stdout}"
    );
    assert!(
        stdout.contains("Run it with:") && stdout.contains("cellar launch balatro"),
        "the summary ends with the next command:\n{stdout}"
    );
    let json = list_json(&xdg);
    assert!(
        json.contains("\"slug\": \"balatro\"") && json.contains("\"prefix\": \"default\""),
        "the registration is durable:\n{json}"
    );
}

/// Write an executable shell stub (a fake `wine` the discover-only
/// provider resolves via PATH).
#[cfg(unix)]
fn write_stub(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write stub");
    let mut perms = fs::metadata(path).expect("stub metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod stub");
}

#[test]
#[cfg(unix)]
fn popup_installer_invocation_runs_and_never_registers_silently() {
    // "Open with Cellar" on a setup.exe: the hint picks the installer
    // branch (announced), the session runs the installer inside the
    // `default` prefix, discovery finds its Desktop exe — and with no TTY
    // and no review flags, nothing registers. The hard rule holds in the
    // popup context too: never a silent registration.
    let xdg = xdg_home("installer");
    let bin = xdg.join("bin");
    fs::create_dir_all(&bin).expect("mkdir bin");
    let wine = bin.join("wine");
    // Plants an exe on the prefix's Desktop and reports success — the
    // installer-run loop in one file, found via PATH (execvp semantics).
    write_stub(
        &wine,
        "mkdir -p \"$WINEPREFIX/drive_c/users/me/Desktop\"\n\
         echo MZ > \"$WINEPREFIX/drive_c/users/me/Desktop/game.exe\"\n\
         exit 0\n",
    );
    let setup = xdg.join("setup.exe");
    fs::write(&setup, "MZ-setup").expect("write installer");

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = cellar();
    command.arg("install").arg(&setup).env("PATH", path);
    let output = spawn(&mut command, &xdg);
    assert_eq!(output.status.code(), Some(0), "the popup run succeeds");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_no_prompts(&stdout, &stderr);
    assert!(
        stderr.contains(
            "Treating 'setup.exe' as installer (hint from the file name — \
             pass --artifact to override)"
        ),
        "the installer branch is hinted, never silently fixed:\n{stderr}"
    );
    assert!(
        stdout.contains("Ran installer 'setup.exe' in prefix 'default'"),
        "the installer ran inside the default prefix:\n{stdout}"
    );
    assert!(
        stdout.contains("Registered nothing in prefix 'default'"),
        "the empty review is a plain outcome, not a silent registration:\n{stdout}"
    );
    let json = list_json(&xdg);
    assert!(
        json.contains("[]"),
        "no entry registered without confirmation:\n{json}"
    );
}

#[test]
fn popup_invocation_with_decision_flags_drives_everything() {
    // The association may pass flags in the exec line — then everything is
    // decided by them: no prompt, no hint announcement, the given prefix
    // and branch taken as-is.
    let xdg = xdg_home("flagged");
    fs::create_dir_all(&xdg).expect("mkdir data home");
    let exe = xdg.join("tool.exe");
    fs::write(&exe, "MZ").expect("write exe");

    let output = popup(
        &xdg,
        &[
            "--prefix",
            "games",
            "--artifact",
            "standalone",
            exe.to_str().expect("utf-8 path"),
        ],
    );
    assert_eq!(output.status.code(), Some(0), "the flagged popup succeeds");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_no_prompts(&stdout, &stderr);
    assert!(
        !stderr.contains("hint from the file name"),
        "a given --artifact leaves nothing to hint:\n{stderr}"
    );
    assert!(
        stdout.contains("Registered 'tool' (game) in prefix 'games'"),
        "the flags decided the whole session:\n{stdout}"
    );
    let json = list_json(&xdg);
    assert!(
        json.contains("\"slug\": \"tool\"") && json.contains("\"prefix\": \"games\""),
        "the flagged registration is durable:\n{json}"
    );
}
