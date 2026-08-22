# Single-root storage tree at `$XDG_DATA_HOME/cellar`

Status: accepted (wayfinder #19, 2026-08)

Cellar keeps its entire source-of-truth file tree — settings, prefixes, apps, and the managed runtime inventory — under one root, `$XDG_DATA_HOME/cellar` (default `~/.local/share/cellar`), deliberately deviating from the XDG Base Directory spec's letter (which would put config files in `$XDG_CONFIG_HOME`). The whole tree is one movable unit: prefix directories (potentially gigabytes) and the metadata describing them must move together, and one root matches the umu precedent (`~/.local/share/umu`) that the managed runtime already follows.

## Considered options

- **Strict XDG split** (settings in `~/.config/cellar`, data in `~/.local/share/cellar`): rejected — relocating a prefix would strand its `.toml` metadata, breaking tree coherence. Revisit only if a read-only or multi-user config need appears.
- **YAML / JSON formats**: rejected — YAML's spec is frozen (no normative changes since 2009) and general-purpose, and its canonical Rust binding (`serde_yaml`) is archived; JSON's grammar has no comments and targets interchange, not human-edited config. Primary-source comparison recorded in wayfinder #19.

## Consequences

- Format locked: TOML (spec v1.1.0, `toml` crate).
- Flat app tree: `apps/<slug>.toml` files carry their `prefix` binding as an ordinary field — the "prefix binding is an override" model made literal (per CONTEXT.md).
- Every file carries `schema_version`; breaking changes migrate via file-tree transforms.
- Writes are atomic (temp + rename) and exclusively the app service's; invalid hand-edited files degrade to skipped entries flagged by `cellar doctor`, never silently overwritten.