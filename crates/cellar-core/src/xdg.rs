//! XDG data-directory resolution, validated (#61): one authority for
//! turning `$XDG_DATA_HOME` / `$HOME` into the data directory, shared by
//! the storage tree and the provider crates (ADR 0002-safe: both depend
//! on core).
//!
//! Two rules, locked in #61: a leading `~/` (and a bare `~`) expands
//! against `$HOME` — the one expansion shell users assume (systemd units
//! and cron pass quoted tildes through) — and whatever path resolves must
//! be ABSOLUTE. A relative value silently rooted second trees under
//! `$PWD`; it now dies loudly, naming the variable to fix.

use std::path::{Path, PathBuf};

/// Why [`data_home`] refused to resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataHomeError {
    /// Neither `XDG_DATA_HOME` nor `HOME` is set (or usable).
    NotSet,
    /// The resolved path was not absolute. Names the variable whose value
    /// is at fault — a relative `HOME` taints the default just as much as
    /// a relative `XDG_DATA_HOME`.
    NotAbsolute {
        variable: &'static str,
        value: String,
    },
}

impl std::fmt::Display for DataHomeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSet => write!(f, "neither XDG_DATA_HOME nor HOME is set"),
            Self::NotAbsolute { variable, value } => write!(
                f,
                "{variable} must be an absolute path; got '{value}' — \
                 set it to something like $HOME/.local/share"
            ),
        }
    }
}

impl std::error::Error for DataHomeError {}

/// Expand a leading `~/` (or bare `~`) against `$HOME`: the one expansion
/// shell users assume (#61). `~user` forms are deliberately untouched.
/// Returns `None` when there is nothing to expand with (no usable `HOME`)
/// or the pattern does not apply.
fn expand_tilde(value: &str, home: Option<&str>) -> Option<PathBuf> {
    let home = home.filter(|home| !home.is_empty())?;
    if value == "~" {
        return Some(PathBuf::from(home));
    }
    let rest = value.strip_prefix("~/")?;
    Some(PathBuf::from(home).join(rest))
}

/// Validate one candidate value: tilde-expand, then require absoluteness.
fn validate(
    variable: &'static str,
    raw: &str,
    home: Option<&str>,
) -> Result<PathBuf, DataHomeError> {
    let resolved = match expand_tilde(raw, home) {
        Some(path) if path.is_absolute() => path,
        Some(_) | None => {
            if Path::new(raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                return Err(DataHomeError::NotAbsolute {
                    variable,
                    value: raw.to_owned(),
                });
            }
        }
    };
    Ok(resolved)
}

/// The XDG data directory: `$XDG_DATA_HOME` when set and non-empty
/// (tilde-expanded, absolute-required), else `$HOME/.local/share` with the
/// same rules applied to `HOME` itself (#61).
///
/// The single authority — the storage tree's `from_env` and every
/// duplicated provider site resolve through this function, so a
/// misconfiguration can never silently point one consumer at `$PWD`.
pub fn data_home() -> Result<PathBuf, DataHomeError> {
    data_home_with(
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// The user's home directory as an absolute path, validated exactly like
/// [`data_home`] validates it: tilde-expanded, absolute-required, named
/// failure. The single authority for `$HOME`-derived paths, so the
/// providers' `$HOME`-rooted discovery (#53) cannot quietly grow a
/// `$PWD`-relative root of its own — a misconfigured environment must
/// yield fewer roots, never a wrong one.
pub fn home_with(home: Option<&str>) -> Result<PathBuf, DataHomeError> {
    match home {
        Some(home) if !home.is_empty() => validate("HOME", home, Some(home)),
        _ => Err(DataHomeError::NotSet),
    }
}

/// [`data_home`] with both variables injected — the seam that keeps the
/// expansion/validation rules testable without touching the process
/// environment.
pub fn data_home_with(
    xdg_data_home: Option<&str>,
    home: Option<&str>,
) -> Result<PathBuf, DataHomeError> {
    match xdg_data_home {
        Some(dir) if !dir.is_empty() => validate("XDG_DATA_HOME", dir, home),
        _ => home_with(home).map(|home| home.join(".local/share")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: Option<&str> = Some("/home/sid");

    #[test]
    fn xdg_data_home_wins_when_absolute() {
        assert_eq!(
            data_home_with(Some("/data"), HOME).unwrap(),
            PathBuf::from("/data")
        );
    }

    #[test]
    fn a_leading_tilde_expands_against_home() {
        assert_eq!(
            data_home_with(Some("~/x"), HOME).unwrap(),
            PathBuf::from("/home/sid/x"),
            "~/ expands"
        );
        assert_eq!(
            data_home_with(Some("~"), HOME).unwrap(),
            PathBuf::from("/home/sid"),
            "bare ~ expands"
        );
    }

    #[test]
    fn a_relative_value_is_refused_naming_the_variable() {
        // AC (#61): the second-tree failure mode dies loudly.
        let error = data_home_with(Some("reldata"), HOME).unwrap_err();
        assert_eq!(
            error,
            DataHomeError::NotAbsolute {
                variable: "XDG_DATA_HOME",
                value: "reldata".to_owned(),
            }
        );
        assert!(
            error.to_string().contains("$HOME/.local/share"),
            "the hint suggests the shape: {error}"
        );
    }

    #[test]
    fn a_relative_home_taints_the_default() {
        // AC (#61): the rule applies to whatever path resolves — a
        // relative HOME producing the default trips the same rule and
        // names HOME.
        let error = data_home_with(None, Some("relhome")).unwrap_err();
        assert_eq!(
            error,
            DataHomeError::NotAbsolute {
                variable: "HOME",
                value: "relhome".to_owned(),
            }
        );
    }

    #[test]
    fn an_unset_xdg_falls_back_to_home_share() {
        assert_eq!(
            data_home_with(None, HOME).unwrap(),
            PathBuf::from("/home/sid/.local/share"),
            "the documented XDG fallback"
        );
        assert_eq!(
            data_home_with(Some(""), HOME).unwrap(),
            PathBuf::from("/home/sid/.local/share"),
            "empty means unset"
        );
    }

    #[test]
    fn home_is_its_own_authority() {
        // The `$HOME`-derived paths discovery builds (Proton's Steam roots,
        // #53) resolve through the same rules as `$XDG_DATA_HOME`, so a
        // relative HOME yields *no* home root rather than a `$PWD`-relative
        // one.
        assert_eq!(home_with(HOME), Ok(PathBuf::from("/home/sid")));
        // A literal `~` as HOME would expand against itself, so the
        // absolute-required rule refuses it rather than inventing a root.
        assert_eq!(
            home_with(Some("~")),
            Err(DataHomeError::NotAbsolute {
                variable: "HOME",
                value: "~".to_owned(),
            })
        );
        assert_eq!(
            home_with(Some("relhome")),
            Err(DataHomeError::NotAbsolute {
                variable: "HOME",
                value: "relhome".to_owned(),
            })
        );
        assert_eq!(home_with(None), Err(DataHomeError::NotSet));
        assert_eq!(home_with(Some("")), Err(DataHomeError::NotSet));
        // The data-home default is exactly home + the XDG share suffix.
        assert_eq!(
            data_home_with(None, HOME),
            Ok(PathBuf::from("/home/sid/.local/share")),
            "the fallback keeps composing from the home authority"
        );
    }

    #[test]
    fn tilde_without_a_usable_home_is_refused() {
        // No usable HOME: `~/x` cannot expand — the raw value is not
        // absolute, so it dies instead of landing somewhere odd.
        assert!(matches!(
            data_home_with(Some("~/x"), None),
            Err(DataHomeError::NotAbsolute { .. })
        ));
    }

    #[test]
    fn neither_variable_set_is_its_own_error() {
        assert_eq!(data_home_with(None, None), Err(DataHomeError::NotSet));
        assert_eq!(data_home_with(None, Some("")), Err(DataHomeError::NotSet));
    }

    #[test]
    fn a_tilde_user_form_is_never_expanded() {
        // Only `~/` and bare `~` expand (#61); `~otheruser` stays literal
        // — and being relative, dies loudly rather than guessing.
        let error = data_home_with(Some("~root/x"), HOME).unwrap_err();
        assert!(
            matches!(error, DataHomeError::NotAbsolute { .. }),
            "~user must not silently expand against $HOME"
        );
    }
}
