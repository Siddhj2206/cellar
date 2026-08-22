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
    /// Order in which runner families are tried during resolution.
    ///
    /// Defaults to empty: selection semantics (app override → prefix default →
    /// kind presets, Game → GE-Proton / Tool → wine) land with the storage
    /// (#26) and launch (#28) slices. Wine is never an automatic fallback for
    /// a failed Proton selection (blueprint §7).
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
    /// Default runner selection (family plus optional pin).
    pub runner: Option<RunnerSpec>,
    /// Environment applied to every launch in this prefix.
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
pub enum AppKind {
    Game,
    Tool,
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
