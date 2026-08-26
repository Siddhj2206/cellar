//! Runner types: families, specs, refs, the `Layer` order, and the plan.

use serde::{Deserialize, Serialize};

use std::collections::BTreeMap;
use std::path::PathBuf;

/// The family of a runner. Selection happens by family first; resolution
/// (configured path → managed install → PATH) happens inside the family's
/// provider (blueprint §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RunnerFamily {
    /// Proton-family runners: managed GE-Proton / umu-Proton, discover-only
    /// Steam Proton. A failed Proton selection never falls back to wine.
    Proton,
    /// Plain system wine, discover-only via PATH.
    Wine,
    /// The umu container launch layer (managed `umu-run`).
    Umu,
}

impl RunnerFamily {
    /// Human-readable identifier, e.g. for CLI tables.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proton => "proton",
            Self::Wine => "wine",
            Self::Umu => "umu",
        }
    }

    /// The prefix.toml spelling (`family = "Wine"`, docs/usage.md) —
    /// configure-a-path hints quote it verbatim.
    pub const fn as_config_str(self) -> &'static str {
        match self {
            Self::Proton => "Proton",
            Self::Wine => "Wine",
            Self::Umu => "Umu",
        }
    }
}

/// A request to resolve a runner: which family, plus any explicit
/// configuration. Providers answer only the specs they service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerSpec {
    pub family: RunnerFamily,
    /// Explicit configuration from selection — app override → prefix default →
    /// defaults floor — resolved in the order configured → managed → PATH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured: Option<ConfiguredRunner>,
}

impl RunnerSpec {
    pub const fn new(family: RunnerFamily) -> Self {
        Self {
            family,
            configured: None,
        }
    }

    pub const fn with_configured(family: RunnerFamily, configured: ConfiguredRunner) -> Self {
        Self {
            family,
            configured: Some(configured),
        }
    }
}

/// An explicit configuration override for a runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfiguredRunner {
    /// A concrete install path (first resolution stage).
    Path(PathBuf),
    /// A version pin for managed installs (selection stage).
    Version(String),
}

/// Whether a provider owns its install (managed) or only reads host state.
/// Mode is trait membership: `ManagedRunner` exists only for managed
/// providers, and a resolved result carries the tag explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProviderMode {
    Managed,
    DiscoverOnly,
}

/// The resolved install state of a runner reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunnerInstall {
    /// A Cellar-owned install: version pin plus the runtime-tree path.
    Managed { version: String, path: PathBuf },
    /// Host state read-only: found path plus optional version.
    Discovered {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
}

/// A reference to a runner: the provider plus its resolved install state
/// (glossary: `RunnerRef`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerRef {
    /// Stable provider identifier, e.g. "proton", "wine", "umu".
    pub provider_id: String,
    pub family: RunnerFamily,
    pub install: RunnerInstall,
}

/// The result of resolving a `RunnerSpec`, tagged with the provider mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRunner {
    pub mode: ProviderMode,
    pub reference: RunnerRef,
}

/// Wrapper layer order (blueprint §5): launch sorts wrappers by this,
/// outermost first — Display (gamescope) → Container (umu) → `RuntimeEnv`.
/// Unknown layers are compile errors by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Layer {
    Display,
    Container,
    RuntimeEnv,
}

impl Layer {
    /// The locked layer set in chain order (outermost first).
    pub const ORDER: [Self; 3] = [Self::Display, Self::Container, Self::RuntimeEnv];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::Container => "container",
            Self::RuntimeEnv => "runtime-env",
        }
    }
}

/// The fully-resolved description of one launch — a pure, printable value
/// (blueprint §7): dry-run, GUI preview, and reproducible argv fall out of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchPlan {
    /// Final command line: argv[0] is the outermost wrapper or the runner.
    pub argv: Vec<String>,
    /// Environment contract: variables the launch adds or overrides.
    pub env: BTreeMap<String, String>,
    /// Working directory, when the launch needs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// The wrapper chain, sorted by `Layer` (Display → Container →
    /// `RuntimeEnv`).
    pub wrappers: Vec<Layer>,
}
