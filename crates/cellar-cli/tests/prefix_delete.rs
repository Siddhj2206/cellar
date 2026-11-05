//! `cellar prefix delete` with apps still bound to it (#41) — the
//! non-interactive half of the contract, black-box against the real binary.
//!
//! A script, a file manager, or a cron job has no terminal to ask on, so
//! these tests pin the three stances the handler takes and cannot drift:
//! the delete is never refused (refusing would break every scripted delete
//! and ADR 0004 gives behaviour no deprecation window), never silent (the
//! bound entries are named on stderr either way), and never a hang (no
//! terminal, no question — and the only lever that skips the question on a
//! terminal is `--force`, which is why `--no-input` deliberately does not
//! exist on this command).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The presentation binary under test.
fn cellar() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cellar"))
}

/// A scratch data home for one test: `$XDG_DATA_HOME` pins where the child
/// builds its tree (ADR 0001) without touching the developer's real one.
fn xdg_home(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("cellar-prefix-delete-{tag}-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).expect("mkdir data home");
    dir
}

/// Run one command the way a script runs it: stdin closed, so the binary
/// sees a non-TTY and knows nobody can answer a question.
fn spawn(command: &mut Command, xdg: &Path) -> Output {
    command
        .stdin(Stdio::null())
        .env("XDG_DATA_HOME", xdg)
        .output()
        .unwrap_or_else(|e| panic!("command failed to spawn: {e}"))
}

/// Register `count` exes in prefix `slug` through the real install flow, so
/// the bindings under test are the ones a user's tree really holds.
fn register(xdg: &Path, slug: &str, names: &[&str]) {
    for name in names {
        let exe = xdg.join(format!("{name}.exe"));
        fs::write(&exe, "MZ").expect("write exe");
        let output = spawn(
            cellar().arg("install").arg(&exe).args([
                "--artifact",
                "standalone",
                "--prefix",
                slug,
                "--no-input",
            ]),
            xdg,
        );
        assert_eq!(
            output.status.code(),
            Some(0),
            "the install registers {}: {}",
            name,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// The prefix's own directory inside the scratch tree — what `prefix delete`
/// removes and nothing else (ADR 0001 ownership).
fn prefix_dir(xdg: &Path, slug: &str) -> PathBuf {
    xdg.join("cellar/prefixes").join(slug)
}

/// The prompt marker of `prefix delete`'s question (`prefix_delete_prompt`
/// in `crates/cellar-cli/src/main.rs`). It must never appear without a
/// terminal: a drifted marker here would silently weaken the test.
const DELETE_PROMPT_MARKER: &str = "Delete prefix '";

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Assert the two-line bound-entry warning, verbatim, naming exactly
/// `slugs`. The wording is the user-visible contract, so it is checked as
/// text, not as "stderr is non-empty".
fn assert_bound_warning(stderr: &str, slug: &str, slugs: &[&str]) {
    let count = slugs.len();
    let (binds, subject) = if count == 1 {
        ("binds", "entry")
    } else {
        ("bind", "entries")
    };
    let expected = format!(
        "{count} {subject} {binds} to prefix '{slug}': {}\n\
         Nothing in that prefix launches once it is gone — recreate it with \
         `cellar prefix create {slug}`, or uninstall the {subject} with `cellar uninstall <slug>`.",
        slugs.join(", ")
    );
    assert!(
        stderr.contains(&expected),
        "the bound entries are named with what the delete costs:\n{stderr}"
    );
}

#[test]
fn deleting_a_populated_prefix_warns_then_deletes_without_asking() {
    // The issue's transcript, non-interactive. The delete goes ahead (never
    // refused), the entries it orphans are named on stderr, no question is
    // asked (there is nobody to answer it), and the prefix's directory is
    // gone — the launch that now fails was at least announced.
    let xdg = xdg_home("bound");
    register(&xdg, "work", &["tool"]);

    let output = spawn(cellar().args(["prefix", "delete", "work"]), &xdg);
    assert_eq!(output.status.code(), Some(0), "the delete is not refused");
    let stdout = stdout_of(&output);
    let stderr = stderr_of(&output);
    assert!(
        !stdout.contains(DELETE_PROMPT_MARKER) && !stderr.contains(DELETE_PROMPT_MARKER),
        "a non-TTY stdin is never asked — that would hang the script:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_bound_warning(&stderr, "work", &["tool"]);
    assert!(
        stdout.contains("Deleted prefix 'work'"),
        "and the delete is still narrated on stdout:\n{stdout}"
    );
    assert!(
        !prefix_dir(&xdg, "work").exists(),
        "exactly that prefix's directory is removed"
    );
}

#[test]
fn force_skips_the_question_and_still_names_the_bound_entries() {
    // `--force` skips the *question* only. Naming is the warning, not the
    // question, so it still prints — that is the whole difference between
    // "I asked and you said yes" and "I told you and you passed --force".
    // `--quiet` is the documented way to ask for no narration.
    let xdg = xdg_home("force");
    register(&xdg, "work", &["helper", "tool"]);

    let output = spawn(cellar().args(["prefix", "delete", "work", "--force"]), &xdg);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_of(&output);
    let stderr = stderr_of(&output);
    assert!(
        !stdout.contains(DELETE_PROMPT_MARKER) && !stderr.contains(DELETE_PROMPT_MARKER),
        "--force skips the question:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_bound_warning(&stderr, "work", &["helper", "tool"]);
    assert!(
        stdout.contains("Deleted prefix 'work'"),
        "the scripted delete proceeds:\n{stdout}"
    );
}

#[test]
fn quiet_silences_the_warning_but_still_deletes() {
    // `--quiet` is narration's lever (ADR 0004), and the bound-entry warning
    // is narration — the same treatment the #63 write-time honesty pass
    // gets. Documented, therefore pinned.
    let xdg = xdg_home("quiet");
    register(&xdg, "work", &["tool"]);

    let output = spawn(cellar().args(["prefix", "delete", "work", "--quiet"]), &xdg);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout_of(&output), "", "--quiet silences the narration");
    assert_eq!(stderr_of(&output), "", "including the bound-entry warning");
    assert!(
        !prefix_dir(&xdg, "work").exists(),
        "and the delete still happened"
    );
}

#[test]
fn deleting_an_unbound_prefix_says_nothing_about_entries() {
    // No entries, no warning: the question is about consequences, and an
    // empty consequence list is not worth two lines of prose on every
    // teardown script's output.
    let xdg = xdg_home("unbound");
    register(&xdg, "work", &["tool"]);
    let output = spawn(cellar().args(["prefix", "create", "spare"]), &xdg);
    assert_eq!(output.status.code(), Some(0));

    let output = spawn(cellar().args(["prefix", "delete", "spare"]), &xdg);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_of(&output);
    let stderr = stderr_of(&output);
    assert!(
        !stderr.contains("binds to prefix") && !stderr.contains(DELETE_PROMPT_MARKER),
        "an unbound prefix warns about nothing:\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("Deleted prefix 'spare'"),
        "and is still narrated:\n{stdout}"
    );
    assert!(
        prefix_dir(&xdg, "work").exists(),
        "the populated prefix is untouched"
    );
}

#[test]
fn no_input_on_prefix_delete_is_a_usage_error() {
    // ADR 0004's semantics rule: a flag that would do nothing on a command
    // is a usage error (exit 2), never silently accepted. `--no-input` would
    // do nothing here — a non-TTY already cannot be asked — and `--force` is
    // the documented lever. Exit 2, so a script that reaches for it learns.
    let xdg = xdg_home("no-input");
    let output = spawn(
        cellar().args(["prefix", "delete", "work", "--no-input"]),
        &xdg,
    );
    assert_eq!(
        output.status.code(),
        Some(2),
        "usage, not a silent no-op: {}",
        stderr_of(&output)
    );
}

#[test]
fn an_orphan_lists_as_broken_prefix_in_json_and_in_the_table() {
    // The second half of the same defect, on the surface the user reads
    // daily: after the delete, `list` must not claim an unlaunchable app is
    // `ok`. Both renderings carry the value — the table is where the lie was
    // read, and the JSON shape is contractual (`docs/cli-json.md`).
    let xdg = xdg_home("orphan");
    register(&xdg, "work", &["tool"]);
    let output = spawn(cellar().args(["prefix", "delete", "work"]), &xdg);
    assert_eq!(output.status.code(), Some(0));

    let json = stdout_of(&spawn(cellar().args(["list", "--json"]), &xdg));
    assert!(
        json.contains("\"slug\": \"tool\"") && json.contains("\"status\": \"broken-prefix\""),
        "the machine shape reports the unreadable bound prefix:\n{json}"
    );
    assert!(
        !json.contains("\"status\": \"ok\""),
        "and never calls the orphan healthy:\n{json}"
    );
    let table = stdout_of(&spawn(cellar().arg("list"), &xdg));
    assert!(
        table.contains("broken-prefix") && table.contains("tool"),
        "the human table says the same:\n{table}"
    );
}

#[test]
fn a_healthy_entry_keeps_reading_ok() {
    // The addition is additive: the everyday `ok`/`missing-exe` vocabulary is
    // untouched by any of this, so a healthy tree still reads plainly.
    let xdg = xdg_home("healthy");
    register(&xdg, "work", &["tool"]);
    let json = stdout_of(&spawn(cellar().args(["list", "--json"]), &xdg));
    assert!(
        json.contains("\"slug\": \"tool\"") && json.contains("\"status\": \"ok\""),
        "a readable prefix and a present exe still read ok:\n{json}"
    );
    assert!(
        !json.contains("broken-prefix"),
        "and nothing is flagged that is not broken:\n{json}"
    );
}
