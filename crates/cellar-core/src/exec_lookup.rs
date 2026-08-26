//! POSIX `execvp`-style PATH lookup — the shared predicate (#52). Both
//! consumers need the same semantics: the wine provider resolves its own
//! binary through the resolution order's PATH stage, and the wrapper
//! activation rule probes `gamescope` before a plan may name it argv[0]
//! (a plan the host cannot exec must fail at the plan stage, where the
//! doctor sees it too — never raw at spawn).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// A PATH lookup function — the seam the wrapper activation rule takes so
/// tests can fake presence without touching the process environment.
pub type PathLookup = fn(&str) -> Option<PathBuf>;

/// [`find_on_path_in`] over the process environment.
pub fn find_on_path(program: &str) -> Option<PathBuf> {
    find_on_path_in(program, std::env::var_os("PATH").as_deref())
}

/// `execvp` command lookup for `program`: the directories of `path` are
/// searched in order (POSIX.1-2024); an empty component is the current
/// directory; the first entry that is an executable regular file wins. The
/// found path is canonicalized — PATH entries may be relative or symlinks,
/// and the plan must carry the real binary.
pub fn find_on_path_in(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    for dir in std::env::split_paths(path) {
        let candidate = dir.join(program);
        if executable_file(&candidate) {
            return Some(candidate.canonicalize().unwrap_or(candidate));
        }
    }
    None
}

/// `execvp`'s "found and executable" predicate: a regular file with the
/// executable bit set.
pub fn executable_file(path: &Path) -> bool {
    path.is_file() && is_executable(path)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    // Linux-first (ADR 0003: platform is a non-seam): a real port decides
    // what "executable" means there.
    true
}
