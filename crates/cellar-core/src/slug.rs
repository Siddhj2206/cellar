//! Slug rules for tree file names (blueprint §6): human-readable slugs,
//! validation, and the `-2` dedupe rule. Pure string logic — no I/O, no
//! platform. A slug maps one-to-one to a file name in the tree, so the
//! character set is closed to what every filesystem accepts safely.

use std::collections::BTreeSet;

/// Longest slug the tree accepts (filesystem-friendly, human-readable).
pub const MAX_LEN: usize = 64;

/// Turn a user-visible name into a slug: lowercase, keep ASCII letters,
/// digits, and `_`, collapse every other character to a single `-`, strip
/// leading/trailing `-`, drop non-ASCII characters, truncate to [`MAX_LEN`].
///
/// Every result passes [`is_valid_slug`] unless it is empty.
pub fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut pending_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(ch.to_ascii_lowercase());
        } else {
            pending_dash = true;
        }
    }
    slug.truncate(MAX_LEN);
    slug
}

/// Whether `slug` is safe to use as a tree file name: non-empty, at most
/// [`MAX_LEN`] bytes, starts with a lowercase ASCII letter or digit, and
/// continues with lowercase ASCII letters, digits, `-`, or `_`. Path
/// separators, `.`/`..`-shaped names, hidden files, and uppercase are
/// rejected by construction — a valid slug is also a safe file name.
pub fn is_valid_slug(slug: &str) -> bool {
    let mut chars = slug.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    if slug.len() > MAX_LEN {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// A unique slug for `base` that is not in `taken`: `base` when free, else
/// `base-2`, `base-3`, … (`-2` dedupe, blueprint §6). `base` must already be
/// valid; the dedupe suffix can push the result past [`MAX_LEN`] for
/// pathological near-limit names, in which case downstream validation
/// rejects it loudly rather than colliding.
pub fn dedupe_slug(base: &str, taken: &BTreeSet<String>) -> String {
    debug_assert!(is_valid_slug(base));
    if !taken.contains(base) {
        return base.to_owned();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn taken(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn slugify_lowercases_and_dashes() {
        assert_eq!(slugify("My Games"), "my-games");
        assert_eq!(slugify("MY GAMES"), "my-games");
    }

    #[test]
    fn slugify_collapses_runs_and_strips_edges() {
        assert_eq!(slugify("  My   GAMES!!  "), "my-games");
        assert_eq!(slugify("---foo...bar--"), "foo-bar");
    }

    #[test]
    fn slugify_keeps_existing_slug_characters() {
        assert_eq!(slugify("games-2_default"), "games-2_default");
    }

    #[test]
    fn slugify_drops_non_ascii() {
        assert_eq!(slugify("über"), "ber");
    }

    #[test]
    fn slugify_truncates_long_names() {
        let long = "a".repeat(100);
        assert_eq!(slugify(&long).len(), MAX_LEN);
    }

    #[test]
    fn slugify_empty_name_is_empty() {
        assert!(slugify("!!!").is_empty());
    }

    #[test]
    fn valid_slugs_are_accepted() {
        for slug in ["default", "games-2", "a", "0x1f_2", "my_games"] {
            assert!(is_valid_slug(slug), "{slug} should be valid");
        }
    }

    #[test]
    fn invalid_slugs_are_rejected() {
        for slug in [
            "", "-abc", ".hidden", "..", "a/b", "a b", "ABC", "é", "a/b/c",
        ] {
            assert!(!is_valid_slug(slug), "{slug} should be invalid");
        }
    }

    #[test]
    fn dedupe_keeps_free_base() {
        assert_eq!(dedupe_slug("games", &taken(&[])), "games");
        assert_eq!(dedupe_slug("games", &taken(&["other"])), "games");
    }

    #[test]
    fn dedupe_appends_incrementing_suffix() {
        assert_eq!(dedupe_slug("games", &taken(&["games"])), "games-2");
        assert_eq!(
            dedupe_slug("games", &taken(&["games", "games-2"])),
            "games-3"
        );
        assert_eq!(
            dedupe_slug("games", &taken(&["games", "games-2", "games-3"])),
            "games-4"
        );
    }
}
