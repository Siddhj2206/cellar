//! The disposable icon cache's file name — the one naming fact two adapters
//! must agree on exactly (#46).
//!
//! `cellar-desktop` writes `cache/icons/<name>` when it extracts an exe's
//! icon; the storage retention sweep decides which cached icons are garbage.
//! A rule derived twice could drift, and the drift would be invisible: the
//! sweep would delete live icons and every affected entry would quietly
//! lose its icon. So the name is derived once, here, below both adapters —
//! pure naming logic, no I/O (blueprint §4).

use std::hash::{Hash, Hasher};
use std::path::Path;

/// The cache file name standing for one exe's icon: the hash of the exe's
/// path as `<16 hex digits>.png`. The path is the `AppEntry` identity
/// (blueprint §6), so a rename reuses the icon and a cached file can never
/// be mistaken for another exe's.
///
/// `DefaultHasher` is not guaranteed stable across Rust releases, so a
/// future toolchain change would re-derive every name. That costs one
/// re-extraction per app (`cellar desktop sync` re-derives them all) and
/// loses nothing: the cache is disposable — the same contract that makes
/// deleting `cache/` safe makes renaming its files safe.
pub fn file_name(exe: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    exe.hash(&mut hasher);
    format!("{:016x}.png", hasher.finish())
}

/// Whether `name` is a file name [`file_name`] could have produced: exactly
/// sixteen lowercase hex digits and the `.png` suffix. The retention sweep
/// only ever deletes files it recognizes as its own — a half-written atomic
/// temp file, or something the user dropped in the cache directory, is not
/// Cellar's to remove (the same "only our own names" rule the launcher-entry
/// sweep follows).
pub fn is_file_name(name: &str) -> bool {
    name.strip_suffix(".png").is_some_and(|stem| {
        stem.len() == 16
            && stem
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    #[test]
    fn a_name_is_sixteen_lowercase_hex_digits_and_png() {
        let name = file_name(Path::new("/games/balatro.exe"));
        assert!(is_file_name(&name), "{name} is not its own kind of name");
        assert_eq!(
            Path::new(&name).extension().and_then(|ext| ext.to_str()),
            Some("png"),
            "{name} must be a PNG"
        );
        assert_eq!(name.len(), 16 + ".png".len(), "{name}: fixed width");
    }

    #[test]
    fn the_name_depends_only_on_the_exe_path() {
        // Identity is the exe path (blueprint §6): the same path always
        // names the same cache file, and two exes never share one.
        let exe = Path::new("/games/balatro.exe");
        assert_eq!(file_name(exe), file_name(exe));
        assert_ne!(file_name(exe), file_name(Path::new("/games/poker.exe")));
    }

    #[test]
    fn names_we_never_produced_are_not_our_names() {
        for name in [
            // Uppercase hex, wrong width, wrong suffix, a directory, and
            // the atomic temp file a write leaves behind mid-rename.
            "DEADBEEFDEADBEEF.png",
            "deadbeef.png",
            "deadbeefdeadbeef.png.gz",
            ".deadbeefdeadbeef.png.tmp1234.0",
            "",
        ] {
            assert!(!is_file_name(name), "{name:?} is not our name");
        }
    }

    #[test]
    fn an_absolute_windows_style_path_still_names_one_file() {
        // AppEntries carry whatever canonical path the host resolved; the
        // naming rule is total over paths, so no exe is nameless.
        let windows_like = PathBuf::from("/games/Café/jeu.exe");
        assert!(is_file_name(&file_name(&windows_like)));
    }
}
