//! The execute phase (blueprint §7): spawn the frozen plan. Nothing spawns
//! before this module — resolve → check → plan are pure and printable
//! (#28); the first `exec` happens here.
//!
//! [`spawn`] runs the plan's argv exactly, with the plan's env merged over
//! the parent's and the plan's cwd when set. Output always lands in the
//! per-launch log the caller chose under `cache/launch-logs` (blueprint §7,
//! §6): [`LaunchMode::Foreground`] pipes the child's stdout/stderr and
//! [`SpawnedProcess::wait`] drains them into the log while mirroring them to
//! our own streams (the CLI's "may forward it"); [`LaunchMode::Detached`]
//! hands the log to the child directly and returns a handle that needs no
//! `wait` — the presentation decides wait-vs-detach (§7).
//!
//! Failures map to the blueprint §7 Spawn family: the OS error is reported
//! as-is, [`LaunchError::Spawn`]. The Runtime family (the exit code) is
//! propagated raw by the presentation, never mapped here.

use cellar_core::types::LaunchPlan;

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use crate::error::LaunchError;

/// The presentation's wait-vs-detach policy (blueprint §7): foreground
/// launches are awaited (and their output forwarded); detached launches
/// release the process from the terminal and return while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// The caller awaits the process: pipes its output into the log while
    /// mirroring it to our streams.
    Foreground,
    /// The process runs on its own: output goes straight into the log, the
    /// caller is not expected to `wait`, and the child leaves the terminal's
    /// foreground process group (Ctrl+C no longer reaches it).
    Detached,
}

/// A spawned launch (blueprint §7 execute phase): pid, per-launch log path,
/// and [`SpawnedProcess::wait`]. Dropping the handle never kills the child —
/// the process runs to completion regardless.
#[derive(Debug)]
pub struct SpawnedProcess {
    pid: u32,
    /// argv[0], for Spawn-family error messages after the spawn.
    program: String,
    log_path: PathBuf,
    /// The caller's copy of the log: foreground mode appends drained output
    /// into it; detached mode hands the file to the child and keeps none.
    log: Option<File>,
    child: Child,
}

impl SpawnedProcess {
    /// The child's process id.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The per-launch log under the disposable cache (blueprint §7).
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// Await the process's exit and return its status raw (blueprint §7
    /// Runtime family — the presentation propagates the code). Foreground
    /// launches additionally drain the child's piped output into the log
    /// while mirroring it to our stdout/stderr, so a game that prints
    /// heavily never blocks on a full pipe.
    pub fn wait(mut self) -> Result<ExitStatus, LaunchError> {
        let mut child = self.child;
        let Some(log) = self.log.take() else {
            // Detached: the child owns its log; just await (tests reap).
            return child.wait().map_err(|e| spawn_err(&self.program, &e));
        };
        let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take())
        else {
            return child.wait().map_err(|e| spawn_err(&self.program, &e));
        };
        // Foreground: the child's stdout/stderr are pipes; drain each to
        // EOF into the log while mirroring to the matching stream. Scoped
        // threads borrow the pipes and the log; the scope joins them once
        // the child has exited.
        let stdout_log = log.try_clone().map_err(|e| spawn_err(&self.program, &e))?;
        thread::scope(|scope| {
            scope.spawn(move || copy_out(&mut stdout, stdout_log, io::stdout().lock()));
            scope.spawn(move || copy_out(&mut stderr, log, io::stderr().lock()));
            // The scope joins the drain threads after the child exits — the
            // common case; a grandchild inheriting a pipe keeps the drain
            // alive, the same bounded wait std's `wait_with_output` offers.
            child.wait()
        })
        .map_err(|e| spawn_err(&self.program, &e))
    }
}

/// Spawn the frozen plan (blueprint §7 execute phase): argv[0] as the
/// program, the remaining argv verbatim, `plan.env` merged over the parent
/// environment (the plan's contract is additions/overrides, never a blank
/// slate), and `plan.cwd` when set — exactly what `--dry-run` printed.
/// Output goes to `log_path`, opened here so every failure stays in the
/// Spawn family with the OS error as-is.
pub fn spawn(
    plan: &LaunchPlan,
    log_path: &Path,
    mode: LaunchMode,
) -> Result<SpawnedProcess, LaunchError> {
    let program = plan.argv.first().ok_or_else(|| LaunchError::Spawn {
        program: "<none>".to_owned(),
        error: "the plan carries no argv — it cannot be spawned".to_owned(),
    })?;
    let mut command = Command::new(program);
    command.args(&plan.argv[1..]).envs(&plan.env);
    if let Some(cwd) = &plan.cwd {
        command.current_dir(cwd);
    }
    // One open per launch: foreground keeps the handle for `wait`'s drain,
    // detached hands it to the child. Append, never truncate — the log name
    // is timestamped, and a stray collision must not destroy output.
    let log = File::options()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| spawn_err(program, &e))?;
    let callers_log = match mode {
        LaunchMode::Foreground => {
            // The child gets pipes the caller drains in `wait`; stdin stays
            // on the terminal so interactive games still read input.
            command.stdin(Stdio::inherit());
            command.stdout(Stdio::piped());
            command.stderr(Stdio::piped());
            Some(log)
        }
        LaunchMode::Detached => {
            // The child writes straight into the log, reads nothing, and —
            // on Unix — starts in its own process group: terminal-delivered
            // signals (Ctrl+C) and the terminal's closing no longer reach
            // it. A full session re-parent (setsid) would need libc/unsafe,
            // out of scope; the release from the terminal's *signals* is the
            // practical detachment this slice promises.
            command.stdin(Stdio::null());
            command.stdout(Stdio::from(
                log.try_clone().map_err(|e| spawn_err(program, &e))?,
            ));
            command.stderr(Stdio::from(log));
            #[cfg(unix)]
            command.process_group(0);
            None
        }
    };
    let child = command.spawn().map_err(|e| spawn_err(program, &e))?;
    Ok(SpawnedProcess {
        pid: child.id(),
        program: program.clone(),
        log_path: log_path.to_owned(),
        log: callers_log,
        child,
    })
}

/// Drain one of the child's pipes to EOF, appending every chunk to the log
/// and mirroring it to one of our streams. The mirror is best-effort — a
/// closed terminal (EPIPE) must not fail the capture the log guarantees; a
/// log write failure is the capture failing and propagates.
fn copy_out<R: Read, L: Write, M: Write>(
    from: &mut R,
    mut log: L,
    mut mirror: M,
) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let read = from.read(&mut buf)?;
        if read == 0 {
            return Ok(());
        }
        log.write_all(&buf[..read])?;
        let _ = mirror.write_all(&buf[..read]);
    }
}

/// The Spawn-family mapping: the OS error verbatim, named with the program
/// that failed.
fn spawn_err(program: &str, error: &io::Error) -> LaunchError {
    LaunchError::Spawn {
        program: program.to_owned(),
        error: error.to_string(),
    }
}

/// A stub executable for spawn tests: an executable shell script in a temp
/// dir. The script body is a `sh` string; the returned path is the script's
/// file path.
#[cfg(test)]
fn temp_script(tag: &str, body: &str) -> (PathBuf, PathBuf) {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use std::os::unix::fs::PermissionsExt;

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "cellar-launch-test-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {dir:?}: {e}"));
    let script = dir.join("stub");
    // Write with an explicit handle: create → write_all → `sync_all` → drop,
    // so the script leaves the write-open state (ETXTBSY window) before any
    // exec ever touches it.
    let mut file = File::create(&script).unwrap_or_else(|e| panic!("create script: {e}"));
    file.write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
        .unwrap_or_else(|e| panic!("write script: {e}"));
    file.sync_all()
        .unwrap_or_else(|e| panic!("sync script: {e}"));
    drop(file);
    let mut perms = fs::metadata(&script)
        .unwrap_or_else(|e| panic!("meta: {e}"))
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap_or_else(|e| panic!("chmod: {e}"));
    (dir, script)
}

/// A plan pointing at one stub executable with canned env and cwd.
#[cfg(test)]
fn stub_plan(script: &Path, cwd: &Path, extra_args: &[&str]) -> LaunchPlan {
    use std::collections::BTreeMap;

    LaunchPlan {
        argv: std::iter::once(script.to_string_lossy().into_owned())
            .chain(extra_args.iter().map(|a| (*a).to_string()))
            .collect(),
        env: BTreeMap::from([
            ("CELLAR_TEST_VAR".to_owned(), "from-plan".to_owned()),
            ("WINEPREFIX".to_owned(), "/p/prefixes/default".to_owned()),
        ]),
        cwd: Some(cwd.to_path_buf()),
        wrappers: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{LaunchMode, SpawnedProcess, spawn, stub_plan, temp_script};

    use cellar_core::types::LaunchPlan;

    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A unique log path inside one test's temp directory.
    fn log_path(dir: &Path) -> PathBuf {
        dir.join(format!(
            "launch-{}-{}.log",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn log_text(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read log {}: {e}", path.display()))
    }

    #[test]
    #[cfg(unix)]
    fn foreground_runs_the_plan_exactly_and_propagates_the_raw_exit_code() {
        let (dir, script) = temp_script(
            "foreground",
            // The plan's surfaces, echoed: argv (args beyond our own), the
            // env contract, the cwd, and both streams.
            "echo \"argv=<$*>\"\n\
             echo \"env=<$CELLAR_TEST_VAR>\"\n\
             echo \"cwd=$(pwd)\"\n\
             echo \"out-line\"\n\
             echo \"err-line\" >&2\n\
             exit 7\n",
        );
        let plan = stub_plan(&script, &dir, &["--fullscreen"]);
        let log = log_path(&dir);
        let process =
            spawn(&plan, &log, LaunchMode::Foreground).unwrap_or_else(|e| panic!("spawn: {e}"));
        let status = process.wait().unwrap_or_else(|e| panic!("wait: {e}"));
        assert_eq!(
            status.code(),
            Some(7),
            "the exit code propagates raw, including non-zero"
        );
        let text = log_text(&log);
        assert!(text.contains("argv=<--fullscreen>"), "argv drift:\n{text}");
        assert!(
            text.contains("env=<from-plan>"),
            "the plan's env merged:\n{text}"
        );
        assert!(
            text.contains(&format!("cwd={}", dir.display())),
            "the plan's cwd applied:\n{text}"
        );
        assert!(text.contains("out-line"), "stdout missing:\n{text}");
        assert!(text.contains("err-line"), "stderr missing:\n{text}");
        assert!(
            std::env::var("WINEPREFIX").unwrap_or_default() != "/p/prefixes/default",
            "the plan's env is merged over the parent's, not replacing it"
        );
    }

    #[test]
    #[cfg(unix)]
    fn foreground_propagates_a_clean_exit_as_zero() {
        let (dir, script) = temp_script("clean", "exit 0\n");
        let plan = stub_plan(&script, &dir, &[]);
        let log = log_path(&dir);
        let status = spawn(&plan, &log, LaunchMode::Foreground)
            .and_then(SpawnedProcess::wait)
            .unwrap_or_else(|e| panic!("launch: {e}"));
        assert_eq!(status.code(), Some(0));
    }

    #[test]
    #[cfg(unix)]
    fn detached_returns_while_the_process_still_runs() {
        let (dir, script) = temp_script("detached", "echo started\nsleep 2\necho done\n");
        let plan = stub_plan(&script, &dir, &[]);
        let log = log_path(&dir);
        let process =
            spawn(&plan, &log, LaunchMode::Detached).unwrap_or_else(|e| panic!("spawn: {e}"));
        // The handle returns while the child lives (#29 acceptance: `--detach`
        // returns with the process still running) — /proc is the Linux pid
        // liveness probe; no libc/unsafe needed.
        #[cfg(target_os = "linux")]
        {
            let live = std::fs::metadata(format!("/proc/{}", process.pid())).is_ok();
            assert!(
                live,
                "the detached process (pid {}) must still be running",
                process.pid()
            );
        }
        // Reap for the test: awaiting a detached handle is permitted (the
        // presentation just never does it). The log fills as the child
        // writes directly into it.
        let status = process.wait().unwrap_or_else(|e| panic!("wait: {e}"));
        assert_eq!(status.code(), Some(0));
        let text = log_text(&log);
        assert!(text.contains("done"), "detached output missing:\n{text}");
    }

    #[test]
    #[cfg(unix)]
    fn a_failing_spawn_reports_the_os_error_as_is() {
        let (dir, _) = temp_script("missing", "");
        let missing = dir.join("does-not-exist");
        let plan = stub_plan(&missing, &dir, &[]);
        let err = spawn(&plan, &log_path(&dir), LaunchMode::Foreground)
            .expect_err("exec of a missing program must fail");
        assert!(
            matches!(
                &err,
                crate::LaunchError::Spawn { program, error }
                    if program == &missing.to_string_lossy().into_owned()
                        && error.contains("os error")
            ),
            "the OS error is reported as-is: {err}"
        );
    }

    #[test]
    fn an_empty_plan_cannot_spawn() {
        let plan = LaunchPlan {
            argv: Vec::new(),
            env: BTreeMap::default(),
            cwd: None,
            wrappers: Vec::new(),
        };
        let err = spawn(&plan, Path::new("/tmp/x.log"), LaunchMode::Foreground)
            .expect_err("empty argv must be rejected");
        assert!(
            matches!(
                &err,
                crate::LaunchError::Spawn { program, error }
                    if program == "<none>" && error.contains("no argv")
            ),
            "a plan without argv is a spawn-family failure: {err}"
        );
    }
}
