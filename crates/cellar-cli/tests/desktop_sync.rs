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

/// Point one of our launcher entries at a binary that no longer exists —
/// the moved-or-deleted-binary state (#57) without needing to move the
/// real test binary.
fn sabotage_exec(entry_path: &Path, slug: &str, dead_target: &str) {
    let wrecked: String = fs::read_to_string(entry_path)
        .expect("entry readable")
        .lines()
        .map(|line| {
            if line.starts_with("Exec=") {
                format!("Exec=\"{dead_target}\" launch {slug}\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect();
    fs::write(entry_path, wrecked).expect("entry rewritten");
}

#[test]
fn a_moved_binary_is_flagged_by_doctor_and_repaired_by_sync() {
    // AC (#57): after the binary moves, doctor FAILs on desktop
    // integration naming every dead entry with the sync fix hint; sync
    // from the new location prints the repaired count as ordinary
    // quietable narration and doctor goes green.
    let xdg = xdg_home("moved-binary");
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
    for slug in ["balatro", "icon32"] {
        sabotage_exec(
            &xdg.join(format!("applications/cellar-{slug}.desktop")),
            slug,
            "/nonexistent/bin dir/cellar-gone",
        );
    }

    // Doctor names the damage and its fix; the health exit is 1.
    let out = spawn(&["doctor"], &xdg);
    assert_eq!(out.status.code(), Some(1), "a moved binary fails doctor");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("desktop integration"), "\n{stdout}");
    assert!(stdout.contains("cellar-balatro.desktop"), "\n{stdout}");
    assert!(stdout.contains("cellar-icon32.desktop"), "\n{stdout}");
    assert!(
        stdout.contains("no longer exists") && stdout.contains("run cellar desktop sync"),
        "\n{stdout}"
    );

    // Sync repairs: the receipt line on stdout, ordinary narration.
    let out = spawn(&["desktop", "sync"], &xdg);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Repaired 2 stale launcher entries"),
        "\n{stdout}"
    );
    let out = spawn(&["doctor"], &xdg);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let section_line = stdout
        .lines()
        .find(|line| line.starts_with("desktop integration"))
        .expect("the section renders");
    assert!(
        section_line.ends_with("ok"),
        "the desktop verdict clears after sync ({section_line}):\n{stdout}"
    );
    assert!(
        !stdout.contains("cellar-balatro.desktop") && !stdout.contains("no longer exists"),
        "the dead-entry findings are gone:\n{stdout}"
    );

    // The repaired count is narration: --quiet silences it entirely.
    for slug in ["balatro", "icon32"] {
        sabotage_exec(
            &xdg.join(format!("applications/cellar-{slug}.desktop")),
            slug,
            "/nonexistent/bin dir/cellar-gone",
        );
    }
    let out = spawn(&["desktop", "sync", "-q"], &xdg);
    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout.is_empty(),
        "quiet silences the repaired receipt: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn doctor_passes_desktop_integration_on_a_never_integrated_host() {
    // AC (#57): a fresh tree passes the fifth section clean — there is no
    // derived artifact to be broken yet, even though tree health (rightly)
    // fails on the missing root.
    let xdg = xdg_home("untouched-host");
    fs::create_dir_all(xdg.join("cellar")).expect("empty tree");
    let out = spawn(&["doctor"], &xdg);
    assert_eq!(out.status.code(), Some(1), "an empty tree is not healthy");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("desktop integration") && !stdout.contains("desktop integration\tFAIL"),
        "\n{stdout}"
    );
    let section_line = stdout
        .lines()
        .find(|line| line.starts_with("desktop integration"))
        .expect("the section renders");
    assert!(
        section_line.ends_with("ok"),
        "the untouched host passes clean: {section_line}"
    );
}
