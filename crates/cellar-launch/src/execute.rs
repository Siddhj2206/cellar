//! The execute phase (blueprint §7): spawn the frozen plan. Nothing spawns
//! before this module — resolve → check → plan are pure and printable
//! (#28); the first `exec` happens here.
//!
//! [`spawn`] runs the plan's argv exactly, with the plan's env merged over
//! the parent's and the plan's cwd when set. Output always lands in the
//! per-launch log the caller chose under `cache/launch-logs` (blueprint §7,
//! §6): [`LaunchMode::Foreground`] pipes the child's stdout/stderr and
//! [`SpawnedProcess::wait`] drains them into the log while mirroring them to
//! our own streams (the CLI's "may forward it") — a drain bounded by a grace
//! window past the child's exit, so a grandchild that inherited a pipe
//! cannot hang the launch after the game is done (#50);
//! [`LaunchMode::Detached`] hands the log to the child directly and returns
//! a handle that needs no `wait` — the presentation decides wait-vs-detach
//! (§7).
//! Failures map to the blueprint §7 Spawn family: the OS error is reported
//! as-is, [`LaunchError::Spawn`]. The Runtime family (the exit code) is
//! propagated raw by the presentation, never mapped here.

use cellar_core::types::LaunchPlan;

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

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

/// How the foreground drain ended: both pipes reached EOF (the common
/// launch), or the grace expired with a pipe still held open by a process
/// that outlived the game (#50).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainOutcome {
    Complete,
    Truncated,
}

/// The post-exit grace window for in-flight output (#50): bounded so a
/// lingering child can never hang the launch, long enough for a live
/// writer's final flushes.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

/// The stderr diagnostic when the grace expired with a pipe still held —
/// a launch that drained cleanly prints nothing (#50).
const TRUNCATION_NOTE: &str =
    "cellar: game exited; output truncated — a child process is still holding the output pipe";

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
    pub fn wait(self) -> Result<ExitStatus, LaunchError> {
        let (status, outcome) = self.wait_draining()?;
        if outcome == DrainOutcome::Truncated {
            // Survives `-q`: a diagnostic about lost capture, not
            // narration (#50).
            let _ = writeln!(io::stderr(), "{TRUNCATION_NOTE}");
        }
        Ok(status)
    }

    /// [`SpawnedProcess::wait`] with the drain outcome exposed — the seam
    /// the truncation note is glued onto, observable in tests.
    fn wait_draining(mut self) -> Result<(ExitStatus, DrainOutcome), LaunchError> {
        let mut child = self.child;
        let Some(log) = self.log.take() else {
            // Detached: the child owns its log; just await (tests reap).
            let status = child.wait().map_err(|e| spawn_err(&self.program, &e))?;
            return Ok((status, DrainOutcome::Complete));
        };
        let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take())
        else {
            let status = child.wait().map_err(|e| spawn_err(&self.program, &e))?;
            return Ok((status, DrainOutcome::Complete));
        };
        // Foreground: the child's stdout/stderr are pipes. Each drain runs
        // detached on its own thread and reports EOF over a channel — the
        // scope-less shape is the point (#50): joining the drains waits for
        // pipe EOF, and a grandchild that inherited the pipe owns that EOF,
        // not the game. After the child exits, the drains share one grace
        // window; whatever misses it is abandoned (its late reads can only
        // ever append to the log, and process exit reaps the stray), and
        // the expiry surfaces as a truncation.
        let stdout_log = log.try_clone().map_err(|e| spawn_err(&self.program, &e))?;
        let (drained_tx, drained_rx) = mpsc::channel();
        let out_tx = drained_tx.clone();
        let err_tx = drained_tx.clone();
        drop(drained_tx);
        thread::spawn(move || {
            let _ = copy_out(&mut stdout, stdout_log, io::stdout().lock());
            let _ = out_tx.send(());
        });
        thread::spawn(move || {
            let _ = copy_out(&mut stderr, log, io::stderr().lock());
            let _ = err_tx.send(());
        });
        let status = child.wait().map_err(|e| spawn_err(&self.program, &e))?;
        let outcome = drain_grace(&drained_rx);
        Ok((status, outcome))
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

/// Collect the drains' EOF signals within one shared grace window: every
/// signal inside the window keeps waiting for the next; the first timeout
/// is a truncation (#50).
fn drain_grace(drained: &mpsc::Receiver<()>) -> DrainOutcome {
    let deadline = Instant::now() + DRAIN_GRACE;
    for _ in 0..2 {
        match drained.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => return DrainOutcome::Truncated,
            Err(RecvTimeoutError::Disconnected) => return DrainOutcome::Complete,
        }
    }
    DrainOutcome::Complete
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
    use super::{
        DrainOutcome, LaunchError, LaunchMode, SpawnedProcess, spawn, stub_plan, temp_script,
    };

    use cellar_core::types::LaunchPlan;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use std::process::Command;
    use std::sync::{
        LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    };
    use std::thread;
    use std::time::Duration;

    /// Serializes every test that keeps a pipe-holding descendant alive
    /// (and the clean-drain test, whose grace window their holders would
    /// starve): while such a descendant lives, this sandbox can freeze the
    /// whole test binary's wall clock, so one holder at a time, and never
    /// under a concurrent grace window.
    static PIPE_HOLDER_SERIES: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Poll for log content: the drain threads resume on the sandbox's
    /// schedule, not ours — capture lands shortly after, and the assert
    /// must not race it.
    fn wait_log_contains(path: &Path, needle: &str) -> String {
        let mut text = String::new();
        for _ in 0..100 {
            text = log_text(path);
            if text.contains(needle) {
                return text;
            }
            thread::sleep(Duration::from_millis(10));
        }
        text
    }

    /// Kill the pipe holder a test spawned (its script recorded the pid) and
    /// give the reaper a moment: in this sandbox a surviving descendant keeps
    /// wall-clock time frozen for this whole test binary, which would bleed
    /// spurious grace-window expiry into any concurrently running test.
    fn kill_holder(dir: &Path) {
        let pid_file = dir.join("holder.pid");
        if let Ok(pid) = std::fs::read_to_string(&pid_file) {
            let _ = Command::new("kill").arg("-9").arg(pid.trim()).status();
            thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_file(&pid_file);
    }

    /// The series lock above plus this reaper bound a holder's whole life
    /// inside its own test.
    fn holder_series_lock() -> std::sync::MutexGuard<'static, ()> {
        PIPE_HOLDER_SERIES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Spawn with a bounded retry on `ETXTBSY`: under this sandbox's /tmp
    /// an exec can race the just-closed write handle of `temp_script` (the
    /// classic "Text file busy" copy-up), purely environmental — the stub
    /// is complete and closed before any spawn call is even made.
    fn spawn_stub(plan: &LaunchPlan, log: &Path, mode: LaunchMode) -> SpawnedProcess {
        let mut attempts = 0;
        loop {
            match spawn(plan, log, mode) {
                Err(LaunchError::Spawn { error, .. })
                    if attempts < 40 && error.contains("Text file busy") =>
                {
                    attempts += 1;
                    thread::sleep(Duration::from_millis(25));
                }
                other => return other.unwrap_or_else(|e| panic!("spawn: {e}")),
            }
        }
    }

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
        let process = spawn_stub(&plan, &log, LaunchMode::Foreground);
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
        let status = spawn_stub(&plan, &log, LaunchMode::Foreground)
            .wait()
            .unwrap_or_else(|e| panic!("launch: {e}"));
        assert_eq!(status.code(), Some(0));
    }

    #[test]
    #[cfg(unix)]
    fn foreground_returns_promptly_when_a_grandchild_holds_the_pipe() {
        // The exe backgrounds a sleeper that outlives it (which inherits
        // both pipes) and exits — real Windows games leave launcher helpers
        // and crash handlers behind constantly (#50). The wait is bounded
        // by the child plus the grace window, never by the sleeper. (The
        // holder is one second, not thirty: long enough to outrun the
        // 500ms grace, short enough not to tax the test sandbox, which
        // keeps reaping a process's descendants before finishing it.)
        let _series = holder_series_lock();
        let (dir, script) = temp_script(
            "lingering",
            "echo before-exit\nsh -c 'echo $$ > holder.pid; sleep 1' &\nexit 0\n",
        );
        let plan = stub_plan(&script, &dir, &[]);
        let log = log_path(&dir);
        let started = std::time::Instant::now();
        let process = spawn_stub(&plan, &log, LaunchMode::Foreground);
        let (status, outcome) = process
            .wait_draining()
            .unwrap_or_else(|e| panic!("wait: {e}"));
        let elapsed = started.elapsed();
        assert_eq!(status.code(), Some(0));
        assert!(
            elapsed < Duration::from_secs(5),
            "a lingering child must not hold the launch hostage: {elapsed:?}"
        );
        assert_eq!(outcome, DrainOutcome::Truncated);
        let text = wait_log_contains(&log, "before-exit");
        assert!(
            text.contains("before-exit"),
            "output recorded before the exit is captured"
        );
        // The holder must not outlive the test: a surviving descendant
        // freezes wall-clock time for this whole test binary here.
        kill_holder(&dir);
    }
    #[test]
    #[cfg(unix)]
    fn output_flushing_within_the_grace_window_is_still_captured() {
        // One orphan flushes a line shortly after the parent exited, then a
        // second orphan keeps the pipe open: the grace captures the flush
        // and still reports the truncation (#50).
        let _series = holder_series_lock();
        let (dir, script) = temp_script(
            "grace-flush",
            "echo parent-line\nsh -c 'sleep 0.2; echo late-flush' &\n\
             sh -c 'echo $$ > holder.pid; sleep 1' &\nexit 0\n",
        );
        let plan = stub_plan(&script, &dir, &[]);
        let log = log_path(&dir);
        let started = std::time::Instant::now();
        let process = spawn_stub(&plan, &log, LaunchMode::Foreground);
        let (_, outcome) = process
            .wait_draining()
            .unwrap_or_else(|e| panic!("wait: {e}"));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wait stayed bounded"
        );
        assert_eq!(outcome, DrainOutcome::Truncated);
        let text = wait_log_contains(&log, "late-flush");
        assert!(text.contains("parent-line"), "pre-exit output:\n{text}");
        assert!(
            text.contains("late-flush"),
            "a flush inside the grace window is captured:\n{text}"
        );
        kill_holder(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn a_clean_drain_reports_complete() {
        // The identical app without a lingering child drains to EOF and
        // reports Complete — exactly the old unbounded shape (#50).
        let _series = holder_series_lock();
        let (dir, script) = temp_script("drain-clean", "echo out\necho err >&2\nexit 0\n");
        let plan = stub_plan(&script, &dir, &[]);
        let log = log_path(&dir);
        let process = spawn_stub(&plan, &log, LaunchMode::Foreground);
        let (status, outcome) = process
            .wait_draining()
            .unwrap_or_else(|e| panic!("wait: {e}"));
        assert_eq!(status.code(), Some(0));
        assert_eq!(outcome, DrainOutcome::Complete);
        let text = log_text(&log);
        assert!(
            text.contains("out") && text.contains("err"),
            "drained:\n{text}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn detached_returns_while_the_process_still_runs() {
        let (dir, script) = temp_script("detached", "echo started\nsleep 2\necho done\n");
        let plan = stub_plan(&script, &dir, &[]);
        let log = log_path(&dir);
        let process = spawn_stub(&plan, &log, LaunchMode::Detached);
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
