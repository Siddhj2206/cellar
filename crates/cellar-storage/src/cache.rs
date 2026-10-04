//! The disposable cache's retention policy (#46) — janitorial, never
//! authoritative.
//!
//! Two kinds of debris accumulate under `cache/` and nothing else collected
//! them:
//!
//! - **per-launch logs** — every launch writes `<slug>-<nanos>.log` and no
//!   run ever deleted one, so the directory only ever grew;
//! - **icons** — an app that was never uninstalled, or whose exe moved,
//!   leaves `cache/icons/<hash>.png` behind forever.
//!
//! Both are decided here, and the invariant is the point: **the sweep only
//! ever removes files inside `cache/launch-logs/` and `cache/icons/`.**
//! Nothing else in the tree is reachable from here — `apps/`, `prefixes/`,
//! `runtime/`, and every entry file are untouched, hand-edit damage is never
//! repaired or erased, and the disposable-cache contract (ADR 0001: the cache
//! subtree is re-derivable and unsynced) is unchanged by the sweep. The
//! removal deliberately does **not** fsync the parent directory either: the
//! cache is unsynced by design, so a removal that a crash reverts costs at
//! most one extra file next sweep.
//!
//! The policy is two rules and two constants, hardcoded on purpose (#46:
//! no `settings.toml` configuration — constants first, configuration when
//! someone asks):
//!
//! - **count per slug** — a launch log is garbage once it is not among the
//!   newest [`LAUNCH_LOGS_PER_SLUG`] of its own slug;
//! - **an age floor** — no log younger than [`LAUNCH_LOG_MIN_AGE`] is ever
//!   removed, however far over the count it is.
//!
//! The floor is the safety property, not a nicety: an in-flight launch is
//! writing its log right now, so its mtime is current and the floor can
//! never make it a removal candidate. It also keeps a bug report
//! reproducible — a report may quote a `--detach` log path weeks later.
//!
//! Cheapness is structural. The log sweep reads one directory non-recursively
//! and only *keeps* a file long enough to age-test it: anything inside the
//! floor is dropped in the pass it is read, so a healthy tree (everything
//! young) allocates nothing, groups nothing and sorts nothing. The icon
//! sweep reads one directory non-recursively and compares names only — and
//! the tree's entry set (the expensive half: one `apps/*.toml` parse per
//! app) is consulted only when something is actually cached, which the
//! caller decides before it gets here.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use cellar_core::errors::StorageError;
use cellar_core::{icon_cache, slug};

/// How many of the most recent logs one slug keeps (#46). "The last few logs
/// of this game" is what a bug report needs — the failing run plus a couple
/// of comparisons (the run before it, one after a fix attempt) — and ten
/// leaves headroom for a report thread without letting one chatty app grow
/// without bound. The cap exists to stop unbounded growth, not to ration
/// bytes: ten small text files per app is nothing on disk.
pub const LAUNCH_LOGS_PER_SLUG: usize = 10;

/// The age floor below which no launch log is ever removed (#46). Four weeks,
/// not days: a report that quotes a log path may be written up weeks after
/// the launch, and the log must still be there even if the app has been
/// launched more than [`LAUNCH_LOGS_PER_SLUG`] times since. A floor in days
/// would make those reports unreproducible; the count cap already bounds what
/// a busy app keeps, so the floor costs only a little disk in the meantime.
pub const LAUNCH_LOG_MIN_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// One launch log the sweep considered: the slug it counts under, the
/// creation stamp its name carries (a deterministic tiebreak), when it was
/// last written (the age floor's clock — an actively appended log is young
/// by construction), and the path.
#[derive(Debug)]
struct LogFile {
    slug: String,
    nanos: u64,
    modified: SystemTime,
    path: PathBuf,
}

/// Remove the launch logs this policy calls garbage: everything past the
/// newest [`LAUNCH_LOGS_PER_SLUG`] of its own slug, and nothing younger than
/// [`LAUNCH_LOG_MIN_AGE`].
///
/// One non-recursive directory read. Files inside the age floor are discarded
/// in the pass that reads them (the floor is absolute), so the grouping and
/// sorting below only ever see files that are old enough to be removable.
pub(crate) fn sweep_launch_logs(dir: &Path, now: SystemTime) -> Result<(), StorageError> {
    let aged = aged_logs(dir, now)?;
    let mut by_slug: BTreeMap<&str, Vec<&LogFile>> = BTreeMap::new();
    for log in &aged {
        by_slug.entry(&log.slug).or_default().push(log);
    }
    let mut first_error = None;
    for logs in by_slug.values_mut() {
        // Newest first: most recently written, then the creation stamp in
        // the name, which is unique per launch — so the order is total and
        // two files sharing an mtime still have a defined rank.
        logs.sort_by(|a, b| b.modified.cmp(&a.modified).then(b.nanos.cmp(&a.nanos)));
        for log in logs.iter().skip(LAUNCH_LOGS_PER_SLUG) {
            note(&mut first_error, remove(&log.path));
        }
    }
    // Every per-file failure is collected rather than returned at once: one
    // file Cellar may not remove (another user's, a busy mount) must not
    // wedge the rest of the directory on every future launch.
    first_error.map_or(Ok(()), Err)
}

/// The launch logs old enough to be considered at all — one directory read,
/// one pass, and the age filter applied before anything is grouped.
///
/// A name that is not `<slug>-<nanos>.log` is not Cellar's log and is never
/// a candidate (the same "only our own names" rule the icon sweep and the
/// launcher-entry sweep follow): the sweep refuses to guess at debris.
fn aged_logs(dir: &Path, now: SystemTime) -> Result<Vec<LogFile>, StorageError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        // No log directory yet: nothing has been logged, nothing to prune.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(storage_io(dir, &error)),
    };
    let mut aged = Vec::new();
    for entry in entries {
        // An entry that cannot even be read is skipped, matching every other
        // walk here; the directory-level failure above is what propagates.
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((slug_name, nanos)) = parse_log_name(name) else {
            continue;
        };
        // `metadata` (not `file_type`) — a launch log is a plain file, and a
        // symlink planted in the cache is not one: skip anything that is not.
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        // A timestamp in the future (clock skew, an fs without mtimes) reads
        // as "young": keep it. `duration_since` errors there, and the floor's
        // meaning is the safe direction.
        if !now
            .duration_since(modified)
            .is_ok_and(|age| age >= LAUNCH_LOG_MIN_AGE)
        {
            continue;
        }
        aged.push(LogFile {
            slug: slug_name.to_owned(),
            nanos,
            modified,
            path: entry.path(),
        });
    }
    Ok(aged)
}

/// The `<slug>-<nanos>.log` naming the launch path writes (`launch_log_path`
/// in `cellar-app`), split back into its two parts: the slug the retention
/// counts under, and the creation stamp. `None` for anything else.
fn parse_log_name(name: &str) -> Option<(&str, u64)> {
    let stem = name.strip_suffix(".log")?;
    // Last dash, not first: slugs may contain one (`my-game`, `games-2`).
    let (slug_name, nanos) = stem.rsplit_once('-')?;
    // Validating the slug also proves the name carries no separator, no
    // `..`, no uppercase — the sweep only ever joins a name it can prove is
    // one tree file name.
    if !slug::is_valid_slug(slug_name) {
        return None;
    }
    Some((slug_name, nanos.parse().ok()?))
}

/// Remove the cached icons no registered app resolves to (#46): every
/// `cache/icons/<name>` that is not the name of some `AppEntry`'s exe.
///
/// `live` is that set — the names derived from the tree's entries by the
/// caller, which is what makes the liveness rule *derived from the real
/// naming scheme* rather than a guess: an icon's name is a pure function of
/// its exe's path (`core::icon_cache`), so a name outside the live set can
/// never be resolved to by any entry, present or future.
///
/// A directory that does not exist is nothing to prune; a file whose name is
/// not one `core::icon_cache` could have produced is not Cellar's to remove.
/// Names are compared, never opened — an icon is never read to decide whether
/// to keep it.
pub(crate) fn sweep_orphan_icons(dir: &Path, live: &BTreeSet<String>) -> Result<(), StorageError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(storage_io(dir, &error)),
    };
    let mut first_error = None;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !icon_cache::is_file_name(name) || live.contains(name) {
            continue;
        }
        note(&mut first_error, remove(&entry.path()));
    }
    first_error.map_or(Ok(()), Err)
}

/// Remove one cache file the sweep owns. A missing file is fine (a
/// concurrent sweep or an uninstall already took it); any other failure is
/// recorded, never fatal to the caller.
fn remove(path: &Path) -> Result<(), StorageError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(storage_io(path, &error)),
    }
}

/// Keep the first failure and let the sweep go on: a sweep that stops at one
/// unremovable file would retry that file — and only that file — on every
/// future launch, forever.
fn note(slot: &mut Option<StorageError>, result: Result<(), StorageError>) {
    if slot.is_none() {
        *slot = result.err();
    }
}

/// The storage taxonomy for a filesystem error at `path`.
fn storage_io(path: &Path, error: &std::io::Error) -> StorageError {
    if error.kind() == std::io::ErrorKind::NotFound {
        StorageError::NotFound(path.display().to_string())
    } else {
        StorageError::Io(format!("{}: {error}", path.display()))
    }
}

/// One launch-log fixture aged by `age`, named with the creation stamp `rank`
/// nanos (the form `launch_log_path` writes, and the sweep's deterministic
/// tiebreak). The age floor is the one thing the tests must control, and
/// `File::set_times` controls it without adding a dev-dependency for a
/// timestamp.
#[cfg(test)]
pub(crate) fn write_aged_log(dir: &Path, slug_name: &str, rank: u64, age: Duration) -> PathBuf {
    let path = dir.join(format!("{slug_name}-{rank}.log"));
    fs::write(&path, "output").unwrap_or_else(|e| panic!("write {path:?}: {e}"));
    let modified = SystemTime::now()
        .checked_sub(age)
        .unwrap_or_else(|| panic!("age {age:?} predates the clock"));
    std::fs::File::open(&path)
        .and_then(|file| file.set_times(std::fs::FileTimes::new().set_modified(modified)))
        .unwrap_or_else(|e| panic!("age {path:?}: {e}"));
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    /// The rank argument as the fixtures write it: a loop counter, widened
    /// once here rather than at every call site.
    fn rank(n: usize) -> u64 {
        u64::try_from(n).expect("fixture rank fits")
    }

    fn write_log(dir: &Path, slug_name: &str, rank_at: usize, age: Duration) -> PathBuf {
        write_aged_log(dir, slug_name, rank(rank_at), age)
    }

    fn logs_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("cellar-sweep-{tag}-{}-{seq}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {dir:?}: {e}"));
        dir
    }

    /// Well past the floor: a log the count rule may remove.
    fn old() -> Duration {
        LAUNCH_LOG_MIN_AGE + Duration::from_secs(86_400)
    }

    /// Comfortably inside the floor: a log nothing may remove.
    fn young() -> Duration {
        Duration::from_secs(86_400)
    }

    /// The launch logs still in `dir` (the names the sweep recognizes), as
    /// `(slug, creation stamp)` pairs sorted by slug then stamp — the order
    /// the tests read best in. Foreign files are invisible here by design,
    /// so each test asserts on them separately.
    fn names(dir: &Path) -> Vec<(String, u64)> {
        let mut logs: Vec<(String, u64)> = fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("read {dir:?}: {e}"))
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter_map(|name| parse_log_name(&name).map(|(slug, nanos)| (slug.to_owned(), nanos)))
            .collect();
        logs.sort_unstable();
        logs
    }

    #[test]
    fn past_the_per_slug_count_only_the_newest_survive() {
        // Fourteen logs of one app, all well past the age floor: the newest
        // [`LAUNCH_LOGS_PER_SLUG`] stay, the rest of that slug's history goes.
        let dir = logs_dir("per-slug-count");
        for rank in 0..14 {
            write_log(&dir, "balatro", rank, old());
        }
        sweep_launch_logs(&dir, SystemTime::now()).expect("sweep");
        assert_eq!(
            names(&dir),
            (4..14)
                .map(|at| ("balatro".to_owned(), rank(at)))
                .collect::<Vec<_>>(),
            "the oldest four of this slug's fourteen logs are the garbage"
        );
    }

    #[test]
    fn the_age_floor_spares_a_log_the_count_would_take() {
        // Fourteen logs of one app: eleven past the floor (so the count rule
        // does bite and takes the oldest of them) and three inside it (so
        // the floor is what saves them — they are over the count too).
        let dir = logs_dir("age-floor");
        for rank in 0..11 {
            write_log(&dir, "balatro", rank, old());
        }
        for rank in 11..14 {
            write_log(&dir, "balatro", rank, young());
        }
        sweep_launch_logs(&dir, SystemTime::now()).expect("sweep");
        assert_eq!(
            names(&dir),
            (1..14)
                .map(|at| ("balatro".to_owned(), rank(at)))
                .collect::<Vec<_>>(),
            "one aged log over the count goes; the three inside the floor stay"
        );
    }

    #[test]
    fn a_slug_inside_the_floor_is_never_pruned_at_all() {
        // The cheap path: every log is young, so nothing is grouped, sorted,
        // or removed — over the count or not.
        let dir = logs_dir("all-young");
        for rank in 0..(LAUNCH_LOGS_PER_SLUG + 5) {
            write_log(&dir, "balatro", rank, young());
        }
        sweep_launch_logs(&dir, SystemTime::now()).expect("sweep");
        assert_eq!(
            names(&dir).len(),
            LAUNCH_LOGS_PER_SLUG + 5,
            "nothing inside the age floor is ever removed"
        );
    }

    #[test]
    fn one_slugs_sweep_never_touches_another_slugs_logs() {
        // Two apps, both over the count: each keeps its own newest, and a
        // quiet app next to a loud one is not made a casualty of it.
        let dir = logs_dir("per-slug-isolation");
        for rank in 0..13 {
            write_log(&dir, "balatro", rank, old());
        }
        write_log(&dir, "warpinator", 0, old());
        write_log(&dir, "warpinator", 1, old());
        write_log(&dir, "warpinator", 2, young());
        sweep_launch_logs(&dir, SystemTime::now()).expect("sweep");
        let expected: Vec<(String, u64)> = (3..13)
            .map(|at| ("balatro".to_owned(), rank(at)))
            .chain((0..3).map(|at| ("warpinator".to_owned(), rank(at))))
            .collect();
        assert_eq!(
            names(&dir),
            expected,
            "balatro loses only its own three oldest; the three-launch app keeps all three"
        );
    }

    #[test]
    fn files_that_are_not_launch_logs_are_never_removed() {
        // Debris and foreign files share the directory: a note the user
        // dropped in, an atomic temp file, a name that is not ours to parse.
        let dir = logs_dir("foreign");
        for rank in 0..(LAUNCH_LOGS_PER_SLUG + 3) {
            write_log(&dir, "balatro", rank, old());
        }
        for foreign in ["notes.txt", ".keep", "Balatro-1.log", "balatro.log"] {
            fs::write(dir.join(foreign), "mine").unwrap_or_else(|e| panic!("write {foreign}: {e}"));
        }
        sweep_launch_logs(&dir, SystemTime::now()).expect("sweep");
        for foreign in ["notes.txt", ".keep", "Balatro-1.log", "balatro.log"] {
            assert!(
                dir.join(foreign).exists(),
                "{foreign} is not a launch log the sweep may remove"
            );
        }
        assert_eq!(
            names(&dir).len(),
            LAUNCH_LOGS_PER_SLUG,
            "only the three aged logs of the one recognizable slug went"
        );
    }

    #[test]
    fn a_missing_log_directory_prunes_nothing() {
        let dir = logs_dir("absent").join("never-created");
        assert!(
            sweep_launch_logs(&dir, SystemTime::now()).is_ok(),
            "a tree that never logged has nothing to sweep"
        );
    }

    #[test]
    fn orphan_icons_go_and_live_ones_stay() {
        // Three cached icons, one of them the name of a registered app's exe
        // (derived, not guessed) — and a foreign file that is nobody's icon.
        let dir = logs_dir("icons");
        let live = icon_cache::file_name(Path::new("/games/balatro.exe"));
        let orphan = icon_cache::file_name(Path::new("/games/uninstalled.exe"));
        let moved = icon_cache::file_name(Path::new("/games/moved-away.exe"));
        for name in [&live, &orphan, &moved] {
            fs::write(dir.join(name), b"png").unwrap_or_else(|e| panic!("write {name}: {e}"));
        }
        fs::write(dir.join("notes.txt"), b"mine").expect("foreign file");
        let live_names = BTreeSet::from([live.clone()]);
        sweep_orphan_icons(&dir, &live_names).expect("sweep");
        assert!(dir.join(&live).exists(), "a live app keeps its icon");
        assert!(
            !dir.join(&orphan).exists(),
            "an uninstalled app's icon goes"
        );
        assert!(
            !dir.join(&moved).exists(),
            "an icon whose exe moved and whose entry names the new path goes"
        );
        assert!(dir.join("notes.txt").exists(), "a foreign file stays put");
    }

    #[test]
    fn a_missing_icon_directory_prunes_nothing() {
        let dir = logs_dir("icons-absent").join("never-created");
        assert!(
            sweep_orphan_icons(&dir, &BTreeSet::new()).is_ok(),
            "a tree that never extracted an icon has nothing to sweep"
        );
    }
}
