//! The `desktop sync` honesty contract around damaged app files (#56):
//! a launcher entry whose `apps/<slug>.toml` fails to parse survives
//! the sweep byte-for-byte, one stderr warning per damaged slug — always
//! visible, `--quiet` included — naming the file and pointing at
//! `cellar doctor`, and exit stays 0 (the damage belongs to doctor).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The presentation binary.
fn cellar() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cellar"))
}

/// A scratch data home for one test: `$XDG_DATA_HOME` pins where the
/// child builds its tree (ADR 0001) without touching the developer's
/// real one.
fn xdg_home(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("cellar-sync-damaged-{tag}-{}", std::process::id()));
    // Clean slate for a re-run of the same binary; a fresh dir needs none.
    fs::remove_dir_all(&dir).ok();
    dir
}

/// Run one command under the scratch environment with stdin closed.
fn spawn(args: &[&str], xdg: &Path) -> Output {
    cellar()
        .args(args)
        .stdin(Stdio::null())
        .env("XDG_DATA_HOME", xdg)
        .output()
        .unwrap_or_else(|e| panic!("command failed to spawn: {e}"))
}

#[test]
fn a_damaged_app_file_warns_and_its_entry_survives_even_under_quiet() {
    let xdg = xdg_home("warn-and-survive");
    let exe = xdg.join("balatro.exe");
    let tool = xdg.join("icon32.exe");
    fs::create_dir_all(&xdg).expect("home");
    fs::write(&exe, "MZ").expect("exe");
    fs::write(&tool, "MZ").expect("tool");
    for path in [&exe, &tool] {
        let out = spawn(
            &[
                "install",
                path.to_str().expect("utf-8 path"),
                "--artifact",
                "standalone",
                "--no-input",
            ],
            &xdg,
        );
        assert_eq!(out.status.code(), Some(0), "install succeeds");
    }
    let entry = xdg.join("applications/cellar-icon32.desktop");
    assert!(entry.exists(), "the second app's entry was derived");
    let before = fs::read_to_string(&entry).expect("entry readable");

    // The user's typo: one wrong kind value in a hand edit.
    let app_toml = xdg.join("cellar/apps/icon32.toml");
    let wrecked = fs::read_to_string(&app_toml)
        .expect("app file")
        .replace("kind = \"game\"", "kind = \"GAMME\"");
    assert!(wrecked.contains("GAMME"), "the hand edit broke the file");
    fs::write(&app_toml, wrecked).expect("wreck");

    // Plain sync: exit 0, the entry survives, and the warning names the
    // file and points at doctor — never a silent "stale" removal.
    let out = spawn(&["desktop", "sync"], &xdg);
    assert_eq!(out.status.code(), Some(0), "sync stays green");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("Removed stale entry"),
        "a damaged app's entry is never called stale:\n{stdout}"
    );
    assert!(
        stderr.contains("apps/icon32.toml") && stderr.contains("cellar doctor"),
        "the warning names the file and the fix path:\n{stderr}"
    );
    assert_eq!(
        fs::read_to_string(&entry).expect("entry readable"),
        before,
        "the entry survives byte-for-byte"
    );

    // Quiet sync: narration silenced, the warning still prints.
    let out = spawn(&["desktop", "sync", "-q"], &xdg);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty(), "quiet silences narration entirely");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("apps/icon32.toml") && stderr.contains("cellar doctor"),
        "warnings are diagnostics — quiet never silences them:\n{stderr}"
    );

    // A lost entry cannot come back while the file stays damaged.
    fs::remove_file(&entry).expect("entry removed");
    spawn(&["desktop", "sync", "-q"], &xdg);
    assert!(!entry.exists(), "no parse, no rebuild");

    // Repairing the TOML is the fix: the next sync re-derives normally.
    let fixed = fs::read_to_string(&app_toml)
        .expect("app file")
        .replace("kind = \"GAMME\"", "kind = \"game\"");
    fs::write(&app_toml, fixed).expect("repair");
    let out = spawn(&["desktop", "sync", "-q"], &xdg);
    assert_eq!(out.status.code(), Some(0));
    let repaired = fs::read_to_string(&entry).expect("entry re-derived");
    assert!(
        repaired.contains("Name=icon32\n"),
        "the repaired app's entry re-derives"
    );
}
