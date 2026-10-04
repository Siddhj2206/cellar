//! The cache retention sweep end-to-end (#46): a real `cellar launch` prunes
//! the disposable cache on its way past and says nothing about it.
//!
//! The unit tests pin each rule in isolation; what only the binary can show
//! is that the whole chain is wired — the launch path calls the sweep, the
//! sweep's own outcome never reaches stdout, stderr, or the exit code (it is
//! janitorial, not a result), and the game's exit code still propagates raw
//! through it.
//!
//! The fixture is a scratch `$XDG_DATA_HOME` (ADR 0001) plus a stub `wine` on
//! the child's `PATH`: a `tool` app defaults to the wine family, and nothing
//! about the retention policy needs a real Wine to be exercised.

use std::fs::{self, File, FileTimes};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

use cellar_core::icon_cache::file_name;
use cellar_storage::{LAUNCH_LOG_MIN_AGE, LAUNCH_LOGS_PER_SLUG};

/// The presentation binary.
fn cellar() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cellar"))
}

/// A scratch data home for one test: `$XDG_DATA_HOME` pins where the child
/// builds its tree without touching the developer's real one. Under it, a
/// stub `wine` the child's `PATH` is pointed at.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cellar-retention-{tag}-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("scratch home: {e}"));
    // The stub runner: it ignores the plan's argv and succeeds, so the launch
    // completes as an ordinary wine launch would.
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap_or_else(|e| panic!("stub bin: {e}"));
    write_stub(&bin.join("wine"), "exit 0\n");
    dir
}

#[cfg(unix)]
fn write_stub(path: &Path, body: &str) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let mut file = File::create(path).unwrap_or_else(|e| panic!("stub {}: {e}", path.display()));
    file.write_all(format!("#!/bin/sh\n{body}").as_bytes())
        .unwrap_or_else(|e| panic!("stub {}: {e}", path.display()));
    file.sync_all()
        .unwrap_or_else(|e| panic!("stub {}: {e}", path.display()));
    drop(file);
    let mut perms = fs::metadata(path).expect("stub metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap_or_else(|e| panic!("stub {}: {e}", path.display()));
}

fn run(home: &Path, args: &[&str]) -> Output {
    let path = match std::env::var("PATH") {
        Ok(current) => format!("{}:{current}", home.join("bin").display()),
        Err(_) => home.join("bin").display().to_string(),
    };
    cellar()
        .args(args)
        .stdin(Stdio::null())
        .env("XDG_DATA_HOME", home)
        .env("PATH", path)
        .output()
        .unwrap_or_else(|e| panic!("command failed to spawn: {e}"))
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A launch log aged past the retention floor: what the sweep is allowed to
/// consider. `File::set_times` backdates the fixture without a dependency.
fn seed_log(logs: &Path, slug: &str, rank: u64) -> PathBuf {
    let path = logs.join(format!("{slug}-{rank}.log"));
    fs::write(&path, "old output").unwrap_or_else(|e| panic!("seed {}: {e}", path.display()));
    let old = SystemTime::now() - (LAUNCH_LOG_MIN_AGE + Duration::from_secs(86_400));
    File::open(&path)
        .and_then(|file| file.set_times(FileTimes::new().set_modified(old)))
        .unwrap_or_else(|e| panic!("age {}: {e}", path.display()));
    path
}

#[test]
fn a_launch_sweeps_the_cache_without_a_word_about_it() {
    let home = scratch("launch-sweep");
    let exe = home.join("warpinator.exe");
    fs::write(&exe, "MZ stub").expect("exe");
    let installed = run(
        &home,
        &[
            "install",
            exe.to_str().expect("utf-8 path"),
            "--artifact",
            "standalone",
            "--kind",
            "tool",
            "--no-input",
        ],
    );
    assert_eq!(
        installed.status.code(),
        Some(0),
        "the fixture entry registers: {}",
        stderr_of(&installed)
    );

    // Debris in the cache: this app's own history past the per-slug count, one
    // log belonging to an app nobody registered, and two cached icons — the
    // one this entry resolves to, and one no entry points at.
    let logs = home.join("cellar/cache/launch-logs");
    let seeded = LAUNCH_LOGS_PER_SLUG + 2;
    for rank in 0..seeded {
        seed_log(&logs, "warpinator", u64::try_from(rank).expect("rank fits"));
    }
    seed_log(&logs, "balatro", 1);
    let icons = home.join("cellar/cache/icons");
    fs::create_dir_all(&icons).expect("icon cache");
    // The live icon's name comes from the entry's canonical exe path — the
    // derivation itself, so the fixture cannot drift from the real one.
    let canonical = fs::canonicalize(&exe).expect("canonical exe");
    let live = icons.join(file_name(&canonical));
    let orphan = icons.join(file_name(Path::new("/gone/uninstalled.exe")));
    for icon in [&live, &orphan] {
        fs::write(icon, "png").unwrap_or_else(|e| panic!("icon {}: {e}", icon.display()));
    }

    let launched = run(&home, &["launch", "warpinator"]);
    assert_eq!(
        launched.status.code(),
        Some(0),
        "the game's exit code still propagates raw: {}",
        stderr_of(&launched)
    );

    // The count rule: the two oldest of this slug's own history go, the rest
    // of its history stays, and the launch's own fresh log joins them.
    for rank in 0..2 {
        let gone = logs.join(format!("warpinator-{rank}.log"));
        assert!(!gone.exists(), "{} should have been swept", gone.display());
    }
    for rank in 2..seeded {
        let kept = logs.join(format!("warpinator-{rank}.log"));
        assert!(kept.exists(), "{} should have been kept", kept.display());
    }
    assert!(
        logs.join("balatro-1.log").exists(),
        "another slug's log is never this launch's business"
    );
    assert_eq!(
        fs::read_dir(&logs).expect("log dir").count(),
        // This slug's retained history, the log this launch just wrote, and
        // the one log belonging to a slug that is not registered here.
        LAUNCH_LOGS_PER_SLUG + 1 + 1,
        "exactly the retained history plus this launch's own log"
    );

    // The icon rule: this entry's icon stays, the orphan goes.
    assert!(live.exists(), "a registered app keeps its icon");
    assert!(!orphan.exists(), "an icon no entry resolves to is swept");

    // And it is janitorial: not one word of it reaches the user, on either
    // stream, at any verbosity.
    let spoken = format!(
        "{}{}",
        String::from_utf8_lossy(&launched.stdout),
        stderr_of(&launched)
    )
    .to_lowercase();
    for word in ["sweep", "prune", "pruned", "cache"] {
        assert!(
            !spoken.contains(word),
            "the sweep is not output ({word:?} in): {spoken}"
        );
    }
}
