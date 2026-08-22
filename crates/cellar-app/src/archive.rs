//! Archive artifact handling (blueprint §8: the archive branch of the
//! flagship flow): ZIP extraction into the bound prefix, path-traversal-safe.
//!
//! The install strategy is closed three-armed logic in `app` (blueprint §5 —
//! deliberately not a port); ZIP is the Windows-default archive format this
//! slice supports. Extraction refuses traversal outright — an entry that
//! would escape the destination is an error, never sanitized silently: the
//! session aborts, and the user sees exactly which entry was refused.

use std::fmt;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

/// Extraction failures, in the archive's own vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveError {
    /// The file is not a readable ZIP archive (missing, corrupt, or another
    /// format — tar support lands when a real need appears).
    NotAnArchive(String),
    /// An entry would escape the destination prefix — refused. The entry
    /// name is reported verbatim so the user can inspect the archive.
    Traversal { entry: String },
    /// Any other I/O failure during extraction (open, mkdir, write).
    Io(String),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnArchive(path) => write!(f, "{path}: not a readable ZIP archive"),
            Self::Traversal { entry } => write!(
                f,
                "archive entry {entry:?} would escape the prefix — extraction refused"
            ),
            Self::Io(message) => f.write_str(message),
        }
    }
}

/// Extract a ZIP archive into `dest`. Every entry's path is validated
/// component-wise *before anything is written* — see [`validate_entry_path`]
/// — so a hostile archive can neither climb out of `dest` (`..`), nor land
/// at an absolute location (leading `/`, `\`, or a `C:`-style drive
/// prefix), nor partially extract before the refusal: the whole archive is
/// vetted, then written. Windows-authored zips may use `\` separators;
/// both are normalized. Directory entries are created implicitly through
/// their files.
pub fn extract_zip(archive: &Path, dest: &Path) -> Result<(), ArchiveError> {
    let file = File::open(archive)
        .map_err(|e| ArchiveError::Io(format!("open {}: {e}", archive.display())))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| ArchiveError::NotAnArchive(format!("{}: {e}", archive.display())))?;
    // Pass one: read and vet every entry name. A traversal anywhere in the
    // archive refuses the whole extraction — nothing is written first.
    let mut vetted = Vec::with_capacity(zip.len());
    for index in 0..zip.len() {
        let entry = zip.by_index(index).map_err(|e| {
            ArchiveError::Io(format!("read {} entry {index}: {e}", archive.display()))
        })?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        vetted.push((name.clone(), validate_entry_path(&name)?));
    }
    // Pass two: write the vetted entries.
    for (name, relative) in vetted {
        let mut entry = zip
            .by_name(&name)
            .map_err(|e| ArchiveError::Io(format!("read {name}: {e}")))?;
        let target = dest.join(&relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ArchiveError::Io(format!("mkdir {}: {e}", parent.display())))?;
        }
        let mut out = File::create(&target)
            .map_err(|e| ArchiveError::Io(format!("create {}: {e}", target.display())))?;
        io::copy(&mut entry, &mut out)
            .map_err(|e| ArchiveError::Io(format!("extract {}: {e}", target.display())))?;
    }
    Ok(())
}

/// Validate one archive entry name into a safe relative path: split on both
/// `/` and `\`, drop empty and `.` components, and refuse `..`, absolute
/// roots, and drive prefixes. Anything suspicious is an error — extraction
/// never rewrites a hostile name into a "safe" location, it aborts.
fn validate_entry_path(name: &str) -> Result<PathBuf, ArchiveError> {
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(ArchiveError::Traversal {
            entry: name.to_owned(),
        });
    }
    let mut relative = PathBuf::new();
    for component in normalized.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                return Err(ArchiveError::Traversal {
                    entry: name.to_owned(),
                });
            }
            // A `C:`-style drive prefix (or any colon in a component —
            // also invalid in Windows file names) is an absolute root in
            // disguise.
            _ if component.contains(':') => {
                return Err(ArchiveError::Traversal {
                    entry: name.to_owned(),
                });
            }
            _ => relative.push(component),
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(ArchiveError::Traversal {
            entry: name.to_owned(),
        });
    }
    Ok(relative)
}

/// Test-only: build a ZIP with the given `(name, contents)` pairs — shared
/// by the archive and session tests so the fixture cannot drift.
#[cfg(test)]
pub(crate) fn build_zip(path: &Path, entries: &[(&str, &str)]) {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    let file = std::fs::File::create(path).unwrap_or_else(|e| panic!("create: {e}"));
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    for (name, contents) in entries {
        zip.start_file(*name, options)
            .unwrap_or_else(|e| panic!("start {name}: {e}"));
        zip.write_all(contents.as_bytes())
            .unwrap_or_else(|e| panic!("write {name}: {e}"));
    }
    zip.finish().unwrap_or_else(|e| panic!("finish: {e}"));
}

#[cfg(test)]
mod tests {
    use super::{ArchiveError, build_zip, extract_zip, validate_entry_path};

    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A unique temp directory for one test.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cellar-archive-test-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("mkdir {dir:?}: {e}"));
        dir
    }

    #[test]
    fn extracts_entries_into_their_relative_places() {
        let dir = temp_dir("extract");
        let archive = dir.join("bundle.zip");
        build_zip(
            &archive,
            &[
                ("Game.exe", "MZ"),
                ("data/levels/1.bin", "level"),
                ("readme.txt", "hi"),
                ("nested/./dup.txt", "a"),
                ("nested/dup.txt", "b"),
            ],
        );
        extract_zip(&archive, &dir).unwrap_or_else(|e| panic!("extract: {e}"));
        assert_eq!(
            std::fs::read(dir.join("Game.exe")).unwrap_or_default(),
            b"MZ"
        );
        assert!(dir.join("data/levels/1.bin").is_file());
        assert!(dir.join("readme.txt").is_file());
        // `.` components are dropped; a duplicate path resolves to the last
        // writer, like any file copy.
        assert_eq!(
            std::fs::read(dir.join("nested/dup.txt")).unwrap_or_default(),
            b"b"
        );
    }

    #[test]
    fn normalizes_windows_separators() {
        let dir = temp_dir("backslash");
        let archive = dir.join("dos.zip");
        build_zip(&archive, &[("folder\\Game.exe", "MZ")]);
        extract_zip(&archive, &dir).unwrap_or_else(|e| panic!("extract: {e}"));
        assert!(dir.join("folder/Game.exe").is_file());
        assert!(!dir.join("folder\\Game.exe").exists());
    }

    #[test]
    fn refuses_parent_traversal() {
        let dir = temp_dir("traversal");
        let archive = dir.join("evil.zip");
        build_zip(&archive, &[("../evil.exe", "MZ")]);
        let err = extract_zip(&archive, &dir).expect_err("traversal must be refused");
        assert!(
            matches!(
                &err,
                ArchiveError::Traversal { entry } if entry == "../evil.exe"
            ),
            "the refusing entry is named: {err}"
        );
        assert!(!dir.parent().unwrap().join("evil.exe").exists());
    }

    #[test]
    fn refuses_absolute_and_drive_prefixed_entries() {
        let dir = temp_dir("absolute");
        let archive = dir.join("abs.zip");
        build_zip(
            &archive,
            &[
                ("/etc/evil.exe", "MZ"),
                ("C:\\Windows\\evil.exe", "MZ"),
                ("\\\\server\\share\\evil.exe", "MZ"),
            ],
        );
        // First hostile entry wins the refusal.
        assert!(matches!(
            extract_zip(&archive, &dir),
            Err(ArchiveError::Traversal { .. })
        ));
    }

    #[test]
    fn refuses_backslash_parent_components() {
        let dir = temp_dir("backslash-parent");
        let archive = dir.join("evil2.zip");
        build_zip(&archive, &[("folder\\..\\..\\evil.exe", "MZ")]);
        assert!(matches!(
            extract_zip(&archive, &dir),
            Err(ArchiveError::Traversal { .. })
        ));
    }

    #[test]
    fn a_hostile_archive_extracts_nothing_before_the_refusal() {
        // The hostile entry comes *after* a benign one: the whole archive
        // is vetted before anything is written, so the refusal leaves the
        // destination untouched.
        let dir = temp_dir("hostile");
        let archive = dir.join("hostile.zip");
        build_zip(&archive, &[("Game.exe", "MZ"), ("../evil.exe", "MZ")]);
        let err = extract_zip(&archive, &dir).expect_err("traversal must be refused");
        assert!(
            matches!(
                &err,
                ArchiveError::Traversal { entry } if entry == "../evil.exe"
            ),
            "the refusing entry is named: {err}"
        );
        assert!(
            !dir.join("Game.exe").exists(),
            "no partial extraction before the refusal"
        );
    }

    #[test]
    fn rejects_non_zip_content() {
        let dir = temp_dir("not-zip");
        let archive = dir.join("bundle.zip");
        std::fs::write(&archive, "definitely not a zip").unwrap_or_else(|e| panic!("write: {e}"));
        assert!(matches!(
            extract_zip(&archive, &dir),
            Err(ArchiveError::NotAnArchive(_))
        ));
    }

    #[test]
    fn validates_paths_component_wise() {
        assert!(validate_entry_path("Game.exe").is_ok());
        assert!(validate_entry_path("a/b/Game.exe").is_ok());
        assert!(validate_entry_path("a/./b").is_ok());
        assert!(validate_entry_path("a//b").is_ok());
        assert!(validate_entry_path("../a").is_err());
        assert!(validate_entry_path("a/../../b").is_err());
        assert!(validate_entry_path("/a").is_err());
        assert!(validate_entry_path("a\\..\\b").is_err());
        assert!(validate_entry_path("C:/a").is_err());
        assert!(validate_entry_path("").is_err());
    }
}
