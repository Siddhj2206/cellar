//! The `.desktop` launcher entry: a pure model and renderer following the
//! freedesktop Desktop Entry Specification (1.4), including the Exec
//! field's argument-quoting rules. Pure string logic — no I/O; the
//! adapter writes what `render` produces.

use std::fmt::Write;
use std::path::Path;

/// The freedesktop category a `Game` entry belongs to.
pub(crate) const CATEGORY_GAME: &str = "Game";

/// The freedesktop category a `Tool` entry belongs to.
pub(crate) const CATEGORY_TOOL: &str = "Utility";

/// The launcher entry for one app: every field the adapter knows, as
/// plain values — the renderer turns them into the file text.
pub(crate) struct DesktopEntry<'a> {
    /// The display name — the app's slug (blueprint §6: the slug is the
    /// display/file name).
    pub name: &'a str,
    /// The absolute path of the presentation binary (the composition
    /// root's current exe), quoted per the Exec-field rules.
    pub executable: &'a Path,
    /// The arguments after the binary — the launch slug, written
    /// verbatim (a slug is Exec-safe by construction).
    pub args: &'a [String],
    /// The absolute icon path the entry references, or `None` for an
    /// icon-less entry (the key is then omitted).
    pub icon: Option<&'a Path>,
    /// The freedesktop category, `Game` or `Utility`.
    pub category: &'a str,
}

impl DesktopEntry<'_> {
    /// The file text per the Desktop Entry Specification: the keys Cellar
    /// promises (`Type`, `Name`, `Exec`, `Icon`, `Categories`, `Comment`,
    /// `Terminal`) in a stable order.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "[Desktop Entry]");
        let _ = writeln!(out, "Type=Application");
        let _ = writeln!(out, "Name={}", self.name);
        let _ = writeln!(
            out,
            "Exec={} {}",
            quote_exec_arg(self.executable),
            exec_args(self.args)
        );
        if let Some(icon) = self.icon {
            let _ = writeln!(out, "Icon={}", icon.display());
        }
        let _ = writeln!(out, "Categories={};", self.category);
        let _ = writeln!(out, "Comment=Launch {} through Cellar", self.name);
        let _ = writeln!(out, "Terminal=false");
        out
    }
}

/// One Exec argument: the safe character set is used verbatim; anything
/// else is double-quoted with the spec's escapes — `"`, `` ` ``, `$`,
/// and `\` backslash-escaped, `%` doubled so the field-code parser
/// leaves it literal.
pub(crate) fn quote_exec_arg(arg: &Path) -> String {
    let value = arg.to_string_lossy();
    if value.chars().all(is_safe) {
        return value.into_owned();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for ch in value.chars() {
        match ch {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(ch);
            }
            '%' => quoted.push_str("%%"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// The safe set of an unquoted Exec argument (Desktop Entry Spec §4.2.2:
/// the reserved characters must be quoted).
fn is_safe(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':' | '+' | '=' | '@')
}

/// The space-joined argument list; every element is Exec-safe by
/// construction (slugs), so no quoting is needed.
fn exec_args(args: &[String]) -> String {
    args.join(" ")
}

/// Wire the "Open with Cellar" file association for Windows executables:
/// a no-display launcher whose exec line calls the presentation binary's
/// install entrypoint directly on the file the file manager hands it
/// (`%f`) — no wrapper binary, no shell wrapper (ADR 0004). The
/// association file makes Cellar *available* in a file manager's "Open
/// With" dialog; the user's once-per-environment choice of default (and
/// the `mimeapps.list` record) stays the user's and the file manager's —
/// Cellar never overrides a default selection.
pub(crate) struct AssociationEntry<'a> {
    pub executable: &'a Path,
}

impl AssociationEntry<'_> {
    pub fn render(&self) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Open with Cellar\n\
             Exec={} install %f\n\
             NoDisplay=true\n\
             MimeType=application/x-ms-dos-program;\n\
             Comment=Install this Windows executable through Cellar\n\
             Terminal=false\n",
            quote_exec_arg(self.executable),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_entry(icon: Option<&Path>, category: &str, args: &[String]) -> String {
        DesktopEntry {
            name: "balatro",
            executable: Path::new("/opt/cellar/bin/cellar"),
            args,
            icon,
            category,
        }
        .render()
    }

    #[test]
    fn renders_the_locked_keys_in_order() {
        let args = [String::from("launch"), String::from("balatro")];
        let rendered = render_entry(
            Some(Path::new("/home/me/.local/share/cellar/cache/icons/x.png")),
            CATEGORY_GAME,
            &args,
        );
        assert_eq!(
            rendered,
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=balatro\n\
             Exec=/opt/cellar/bin/cellar launch balatro\n\
             Icon=/home/me/.local/share/cellar/cache/icons/x.png\n\
             Categories=Game;\n\
             Comment=Launch balatro through Cellar\n\
             Terminal=false\n"
        );
    }

    #[test]
    fn an_iconless_entry_omits_the_icon_key() {
        let args = [String::from("launch"), String::from("balatro")];
        let rendered = render_entry(None, CATEGORY_TOOL, &args);
        assert!(!rendered.contains("Icon="), "no Icon key without an icon");
        assert!(rendered.contains("Categories=Utility;"));
    }

    #[test]
    fn exec_quoting_handles_reserved_characters() {
        // A safe absolute path passes through unquoted.
        assert_eq!(
            quote_exec_arg(Path::new("/opt/bin/cellar")),
            "/opt/bin/cellar"
        );
        // Spaces and reserved characters force quoting with escapes.
        assert_eq!(
            quote_exec_arg(Path::new("/home/me/My Games/cellar")),
            "\"/home/me/My Games/cellar\""
        );
        assert_eq!(
            quote_exec_arg(Path::new("/a\"b`c$d\\e")),
            "\"/a\\\"b\\`c\\$d\\\\e\""
        );
        // Literal percent is doubled — the exec parser would otherwise
        // read a field code.
        assert_eq!(quote_exec_arg(Path::new("/x%20y")), "\"/x%%20y\"");
    }

    #[test]
    fn association_entry_uses_the_install_entrypoint_with_percent_f() {
        let rendered = AssociationEntry {
            executable: Path::new("/opt/cellar/bin/cellar"),
        }
        .render();
        assert!(rendered.contains("Exec=/opt/cellar/bin/cellar install %f\n"));
        assert!(rendered.contains("NoDisplay=true\n"));
        assert!(rendered.contains("MimeType=application/x-ms-dos-program;\n"));
    }
}
