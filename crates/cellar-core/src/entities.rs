//! Storage entities: the source-of-truth tree shapes (blueprint §6, ADR 0001)
//! using the glossary vocabulary (CONTEXT.md).

use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::types::{RunnerFamily, RunnerRef, RunnerSpec};

/// Global settings (`settings.toml`): runner resolution order, umu/proton
/// config. Per-file `schema_version` lives in storage's file mapping.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Settings {
    /// The defaults-floor order: when nonempty, `resolution_order[0]`
    /// replaces the kind preset at the defaults floor (blueprint §7:
    /// "defaults floor (settings.toml + kind presets)").
    ///
    /// The floor is kind-driven by default — Game → Proton (GE-Proton),
    /// Tool → wine. Wine is never an *automatic* fallback, so the later
    /// entries are not a fallback chain; the full order's semantics land
    /// once settings have real consumers (#34+).
    pub resolution_order: Vec<RunnerFamily>,
    // umu/proton configuration fields land with the runner-managed slices
    // (#34+), once the settings schema has real consumers.
}

/// A Cellar-managed Windows environment holding the defaults for every
/// `AppEntry` inside it (glossary: Prefix).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prefix {
    /// Human-readable name; file identity (slug, `-2` dedupe).
    pub slug: String,
    pub defaults: PrefixDefaults,
}

/// Prefix-level defaults every `AppEntry` inherits unless overridden
/// (runner, environment, graphics, Windows version).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PrefixDefaults {
    /// Default runner selection (family plus optional pin). Missing in
    /// hand-edited files reads as "no default" — `Option` fields default
    /// by construction.
    pub runner: Option<RunnerSpec>,
    /// Environment applied to every launch in this prefix. Explicitly
    /// defaulted so a hand-edited file may omit it (ADR 0001: human-edited
    /// config; a missing table degrades to empty, never to an invalid
    /// file).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Graphics backend setting (schema lands with the storage slice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graphics: Option<String>,
    /// Windows version compatibility setting (schema lands with the storage
    /// slice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_version: Option<String>,
}

/// Metadata on an `AppEntry` — Game or Tool. The presets hook (Game →
/// GE-Proton, Tool → wine); never a storage location (glossary: kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppKind {
    Game,
    Tool,
}

impl AppKind {
    /// The human label, e.g. for CLI flags and tables.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Game => "game",
            Self::Tool => "tool",
        }
    }

    /// The kind's preset at the defaults floor (blueprint §7): the fallback
    /// family when neither an app override nor a prefix default selects a
    /// runner — Game → Proton (GE-Proton), Tool → wine. Selection walks app
    /// override → prefix default → this floor; the walk lands with the launch
    /// slice (#28), this is the floor itself. Settings-driven floor overrides
    /// land once the schema has consumers (#34+).
    pub const fn default_family(self) -> RunnerFamily {
        match self {
            Self::Game => RunnerFamily::Proton,
            Self::Tool => RunnerFamily::Wine,
        }
    }
}

impl std::str::FromStr for AppKind {
    type Err = String;

    /// The flag/table vocabulary (`game`, `tool`) — the same strings
    /// [`AppKind::as_str`] emits, so CLI parsing and rendering can never
    /// drift.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "game" => Ok(Self::Game),
            "tool" => Ok(Self::Tool),
            other => Err(format!("unknown kind {other:?} — use `game` or `tool`")),
        }
    }
}

/// Per-AppEntry replacement of a prefix default — including the prefix
/// binding, whose default is the prefix that registered the entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Overrides {
    /// Prefix-binding override: which prefix's defaults apply to this entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Runner override (family plus optional pin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<RunnerSpec>,
    /// Environment overrides (highest precedence in env assembly).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// A registered executable the user can launch through Cellar (glossary:
/// `AppEntry`). Identity is the canonical exe path; the slug is the
/// display/file name with `-2` dedupe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppEntry {
    pub slug: String,
    /// Canonical absolute path of the .exe — the entry's identity.
    pub exe: PathBuf,
    pub kind: AppKind,
    /// Prefix binding (the binding is an override by default, ADR 0001).
    pub prefix: String,
    pub overrides: Overrides,
    /// Current-state metadata: the resolved runner when one is pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<RunnerRef>,
    /// Artifact this entry was registered from (installer/archive), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_installer: Option<PathBuf>,
    /// ISO-8601 registration timestamp (current-state metadata).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
}

/// A discovery candidate: an executable found via `.lnk` reading (glossary:
/// Discovery). Users review candidates; nothing auto-registers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub exe: PathBuf,
    /// Human label, typically the shortcut's display name.
    pub label: String,
}

#[cfg(test)]
mod tests {
    use super::AppKind;

    use crate::types::RunnerFamily;

    #[test]
    fn kind_labels_are_lowercase_cli_vocabulary() {
        assert_eq!(AppKind::Game.as_str(), "game");
        assert_eq!(AppKind::Tool.as_str(), "tool");
    }

    #[test]
    fn kind_presets_hold_at_the_defaults_floor() {
        // Blueprint §7: the defaults floor is kind-driven — Game → Proton
        // (GE-Proton), Tool → wine. Launch selection walks app override →
        // prefix default → this floor (#28); the floor itself never moves.
        assert_eq!(AppKind::Game.default_family(), RunnerFamily::Proton);
        assert_eq!(AppKind::Tool.default_family(), RunnerFamily::Wine);
    }
}
