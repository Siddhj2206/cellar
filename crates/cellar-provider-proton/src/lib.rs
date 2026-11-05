//! The Proton provider (blueprint §5): managed GE-Proton / umu-Proton and
//! discover-only Steam Proton.
//!
//! Implements `RunnerResolver` (both modes) and `ManagedRunner` (a declarative
//! manifest for the storage-owned installer pipeline, research #18: SHA-512
//! verification, the `<tag>-<arch>.tar.gz` asset shape, single-root-dir
//! extraction). Resolution follows the locked order (research #18,
//! blueprint §7): configured path → managed install (the tree's
//! `runtime/proton/` versions) → Steam's compatibility dirs (read-only
//! discovery). The scan roots are injected — the wine provider's PATH
//! seam — so resolution stays deterministic and testable without touching
//! the process environment.
//!
//! **Discovery order (#53)** — the order [`steam_roots_from_env`] hands
//! roots to [`ProtonProvider::scan_steam`], and therefore the order
//! resolution reads when several libraries hold Protons:
//!
//! 1. every `compatibilitytools.d` first — an explicitly installed
//!    third-party Proton outranks a Steam-shipped one;
//! 2. then the main library's `steamapps/common`;
//! 3. then each additional library Steam records in `libraryfolders.vdf`,
//!    **in the order that file lists them** — Steam's own priority order,
//!    so following it keeps resolution stable on any host — each library's
//!    `compatibilitytools.d` before its `steamapps/common`.
//!
//! Within a root, directory names sort ascending (as before #53), and
//! every discovered install is canonicalized *before* the dedupe, so a
//! root reached through two overlapping paths (`~/.steam/steam` is usually
//! a symlink into `$XDG_DATA_HOME/Steam`) yields one row, not two.
//!
//! A per-library `compatibilitytools.d` (#53 rule 3) is read as
//! tolerance: Valve documents the Steam root's, and a library one costs
//! only a stat when absent.

use cellar_core::errors::{ResolveError, UnresolvedCause};
use cellar_core::manifest::{
    ArchiveLayout, ChecksumScheme, InstallKind, ReleaseSource, RunnerManifest,
};
use cellar_core::ports::{__sealed, ManagedRunner, RunnerResolver};
use cellar_core::types::{
    ConfiguredRunner, ProviderMode, ResolvedRunner, RunnerFamily, RunnerInstall, RunnerRef,
    RunnerSpec,
};

use std::env;
use std::path::{Path, PathBuf};

/// Managed GE-Proton provider.
#[derive(Debug)]
pub struct ProtonProvider {
    manifest: RunnerManifest,
    /// The tree's `runtime/` directory — the managed-install scan root
    /// (the composition root passes `TreeStore::data_root()/runtime`; the
    /// provider never guesses a data root itself).
    runtime_dir: PathBuf,
    /// Steam's compatibility-tools roots, read-only discovery.
    steam_roots: Vec<PathBuf>,
}

impl ProtonProvider {
    /// Stable provider identifier, shared by the resolver trait and the
    /// managed-runner manifest.
    pub const ID: &'static str = "proton";

    pub fn new() -> Self {
        Self::with_roots(runtime_from_env(), steam_roots_from_env())
    }

    /// Over explicit scan roots (tests, embedded use).
    pub fn with_roots(runtime_dir: PathBuf, steam_roots: Vec<PathBuf>) -> Self {
        Self {
            manifest: RunnerManifest {
                provider_id: Self::ID.to_owned(),
                source: ReleaseSource {
                    url_template: "https://github.com/GloriousEggroll/proton-ge-custom/releases/download/{tag}/{tag}-{arch}.tar.gz"
                        .to_owned(),
                    checksum_url_template: Some(
                        "https://github.com/GloriousEggroll/proton-ge-custom/releases/download/{tag}/{tag}-{arch}.sha512sum"
                            .to_owned(),
                    ),
                    latest_url: Some(
                        "https://github.com/GloriousEggroll/proton-ge-custom/releases/latest"
                            .to_owned(),
                    ),
                },
                checksum: ChecksumScheme::Sha512,
                archive: ArchiveLayout::ExtractsToSingleRootDir,
                install_kind: InstallKind::CompatTool,
            },
            runtime_dir,
            steam_roots,
        }
    }

    /// Whether a directory holds a working Proton install — the probe the
    /// managed scan and the Steam scan both use (an executable `proton`
    /// launcher at the root, research #18: "detect the launcher `proton`
    /// script").
    pub fn proton_dir_ok(dir: &Path) -> bool {
        executable_file(&dir.join("proton"))
    }

    /// Steam Proton discovery (read-only host state, research #18): the
    /// directories under the given roots that hold a working Proton
    /// install — compatibility-tools dirs (`compatibilitytools.d/*`) and
    /// Steam's own `common/Proton *` installs.
    ///
    /// Deterministic order, the rule the module docs state (#53): the
    /// roots in the order given (`steam_roots_from_env` puts every
    /// `compatibilitytools.d` before the `steamapps/common` roots and the
    /// additional libraries in Steam's own `libraryfolders.vdf` order),
    /// directories name-sorted within a root, and each install
    /// canonicalized *before* the dedupe — an install reachable through
    /// two overlapping roots is one row, first root wins.
    pub fn scan_steam(roots: &[PathBuf]) -> Vec<SteamProton> {
        let mut found: Vec<SteamProton> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for root in roots {
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            let mut versions: Vec<(String, PathBuf)> = entries
                .filter_map(Result::ok)
                .map(|entry| {
                    (
                        entry.file_name().to_string_lossy().into_owned(),
                        entry.path(),
                    )
                })
                .filter(|(name, _path)| {
                    if root.file_name().is_some_and(|n| n == "common") {
                        name.starts_with("Proton")
                    } else {
                        true
                    }
                })
                .filter(|(_, path)| Self::proton_dir_ok(path))
                .collect();
            versions.sort_by(|a, b| a.0.cmp(&b.0));
            for (version, dir) in versions {
                // The dedupe runs on the canonical path: `~/.steam/steam` is
                // routinely a symlink into `$XDG_DATA_HOME/Steam`, so the raw
                // paths differ while the install is one (#53).
                let dir = canonical(&dir);
                if seen.insert(dir.clone()) {
                    found.push(SteamProton { dir, version });
                }
            }
        }
        found
    }
}

impl Default for ProtonProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl __sealed::Sealed for ProtonProvider {}

impl RunnerResolver for ProtonProvider {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn resolve(&self, spec: &RunnerSpec) -> Result<ResolvedRunner, ResolveError> {
        Self::resolve_inner(spec, &self.runtime_dir, &self.steam_roots)
    }
}

impl ProtonProvider {
    /// Resolution, research #18 order: configured path → managed install
    /// → Steam compat dirs. A broken configured path falls through (the
    /// locked wording is an *order*, not a hard stop — the wine
    /// provider's precedent); a version pin never falls through — a
    /// pinned-but-missing managed install is `NotInstalled` so a
    /// `SuggestInstall` names the exact reinstall.
    fn resolve_inner(
        spec: &RunnerSpec,
        runtime_dir: &Path,
        steam_roots: &[PathBuf],
    ) -> Result<ResolvedRunner, ResolveError> {
        if spec.family != RunnerFamily::Proton {
            return Err(ResolveError::Unresolvable {
                family: RunnerFamily::Proton,
                cause: UnresolvedCause::NoneFound {
                    mode: ProviderMode::Managed,
                },
            });
        }
        if let Some(configured) = &spec.configured {
            match configured {
                ConfiguredRunner::Path(path) if Self::proton_dir_ok(path) => {
                    return Ok(resolved(proton_ref(path, None)));
                }
                ConfiguredRunner::Path(_) => {}
                ConfiguredRunner::Version(version) => {
                    let install = runtime_dir.join("proton").join(version);
                    if Self::proton_dir_ok(&install) {
                        return Ok(managed_resolved(&install, version));
                    }
                    return Err(ResolveError::NotInstalled {
                        family: RunnerFamily::Proton,
                    });
                }
            }
        }
        // The managed stage: installed versions under the tree's runtime
        // dir, newest install first (a pin selected a *specific* version
        // above; without one, the newest installed wins). The same probe
        // guards every candidate — a half-installed dir is never
        // resolvable. Runtime-tree installs resolve Managed — the result
        // carries the mode tag (blueprint §5).
        if let Some((version, install)) =
            newest_installed(&runtime_dir.join("proton"), Self::proton_dir_ok)
        {
            return Ok(managed_resolved(&install, &version));
        }
        // The discover-only stage: Steam's compatibility layout, read
        // only — Cellar never owns these.
        if let Some(steam) = Self::scan_steam(steam_roots).into_iter().next() {
            return Ok(resolved(proton_ref(&steam.dir, Some(steam.version))));
        }
        // Nothing in the read-only Steam roots (#53). Naming the roots read
        // keeps the failure honest: the install may well exist on a
        // library Cellar was not pointed at, and "install it" is the wrong
        // advice for a Proton that is already on disk.
        Err(ResolveError::Unresolvable {
            family: RunnerFamily::Proton,
            cause: if steam_roots.is_empty() {
                // No roots at all means the environment gave none (a
                // misconfigured XDG/HOME), not that Steam's dirs were read
                // and came up empty — the install/configure wording stands.
                UnresolvedCause::NoneFound {
                    mode: ProviderMode::Managed,
                }
            } else {
                UnresolvedCause::SearchedNothing {
                    searched: steam_roots.to_vec(),
                }
            },
        })
    }
}

/// One discovered Steam Proton install (read-only host state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteamProton {
    pub dir: PathBuf,
    pub version: String,
}

impl ManagedRunner for ProtonProvider {
    fn manifest(&self) -> &RunnerManifest {
        &self.manifest
    }
}

fn proton_ref(path: &Path, version: Option<String>) -> RunnerRef {
    RunnerRef {
        provider_id: ProtonProvider::ID.to_owned(),
        family: RunnerFamily::Proton,
        install: RunnerInstall::Discovered {
            path: path.to_path_buf(),
            version,
        },
    }
}

fn resolved(reference: RunnerRef) -> ResolvedRunner {
    ResolvedRunner {
        mode: ProviderMode::DiscoverOnly,
        reference,
    }
}

/// A runtime-tree install resolves Managed — the result carries the mode
/// tag (blueprint §5): the owner of an artifact is not a *discoverer* of
/// it.
fn managed_resolved(install: &Path, version: &str) -> ResolvedRunner {
    ResolvedRunner {
        mode: ProviderMode::Managed,
        reference: RunnerRef {
            provider_id: ProtonProvider::ID.to_owned(),
            family: RunnerFamily::Proton,
            install: RunnerInstall::Managed {
                version: version.to_owned(),
                path: install.to_path_buf(),
            },
        },
    }
}

/// The newest (by directory mtime) dir under `dir` whose `probe` passes,
/// as `(name, path)`. The shared managed-install scan.
fn newest_installed(dir: &Path, probe: fn(&Path) -> bool) -> Option<(String, PathBuf)> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok();
            (name, path, modified)
        })
        .filter(|(_, path, _)| probe(path))
        .max_by_key(|(_, _, modified)| *modified)
        .map(|(name, path, _)| (name, path))
}

/// The tree's runtime dir from the environment (`$XDG_DATA_HOME/cellar/
/// runtime`, mirroring `TreeStore::from_env`'s XDG fallback) — the
/// default construction inputs; the composition root passes the store's
/// own root explicitly.
fn runtime_from_env() -> PathBuf {
    cellar_core::xdg::data_home().map_or_else(
        |_| PathBuf::from("/cellar/runtime"),
        |data| data.join("cellar").join("runtime"),
    )
}

/// The Steam discovery roots from the environment (research #18: the
/// `compatibilitytools.d` layout and Steam's own `common/Proton *`
/// installs; widened in #53 to Flatpak Steam and to every library Steam
/// records in `libraryfolders.vdf`).
///
/// The order is the discovery order the module docs state (#53):
///
/// 1. every `compatibilitytools.d` — the XDG data home's, then the
///    legacy `~/.steam/steam` one, then Flatpak Steam's `data/Steam` and
///    `.local/share/Steam` spellings;
/// 2. the main library's `steamapps/common` (`~/.steam/steam`, and the
///    same dir under the XDG data home, which is usually what that
///    symlink resolves to);
/// 3. each additional library Steam records in `libraryfolders.vdf`, in
///    that file's order — Steam's own priority order — and each
///    library's `compatibilitytools.d` before its `steamapps/common`.
///
/// Roots that resolve to the same directory are listed once: the dedupe
/// is on the canonical path, so an overlapping layout contributes each
/// install exactly once (the `scan_steam` dedupe does the same per
/// install). Read-only throughout — `libraryfolders.vdf` is opened for
/// reading and nothing is ever created under a Steam root.
pub fn steam_roots_from_env() -> Vec<PathBuf> {
    steam_roots_with(
        env::var("XDG_DATA_HOME").ok().as_deref(),
        env::var("HOME").ok().as_deref(),
    )
}

/// [`steam_roots_from_env`] with both variables injected — the seam the
/// wine provider uses for `PATH`, so the whole root derivation (flatpak
/// spellings, `libraryfolders.vdf`, the dedupe) is testable without
/// mutating the process environment, which edition-2024 `set_var` cannot
/// do without `unsafe` (forbidden workspace-wide).
fn steam_roots_with(xdg_data_home: Option<&str>, home: Option<&str>) -> Vec<PathBuf> {
    // The validated core resolution (#61): a misconfigured environment
    // yields *fewer* roots, never a `$PWD`-relative one. An unresolvable
    // `XDG_DATA_HOME` drops the XDG Steam roots; an unresolvable `HOME`
    // drops the `$HOME`-derived ones (legacy and flatpak) — it never
    // invents a root.
    let data = cellar_core::xdg::data_home_with(xdg_data_home, home).ok();
    let home = cellar_core::xdg::home_with(home).ok();

    let mut roots: Vec<PathBuf> = Vec::new();
    let mut push = |root: PathBuf| {
        let root = canonical(&root);
        if !roots.contains(&root) {
            roots.push(root);
        }
    };

    // Steam itself, in the spellings Steam installs use. The XDG data home
    // first (the documented location, and what research #18 locked);
    // `~/.steam/steam` second because it is the symlink Valve's own docs
    // spell — canonicalizing collapses the two into one root when they are
    // the same directory.
    let mut steam_roots: Vec<PathBuf> = Vec::new();
    if let Some(data) = data.as_ref() {
        steam_roots.push(data.join("Steam"));
    }
    if let Some(home) = home.as_ref() {
        steam_roots.push(home.join(".steam").join("steam"));
    }
    for steam in &steam_roots {
        push(steam.join("compatibilitytools.d"));
    }

    // Flatpak Steam (#53): two spellings for the same app id, both read.
    if let Some(home) = home.as_ref() {
        let flatpak = home
            .join(".var")
            .join("app")
            .join("com.valvesoftware.Steam");
        push(
            flatpak
                .join("data")
                .join("Steam")
                .join("compatibilitytools.d"),
        );
        push(
            flatpak
                .join(".local")
                .join("share")
                .join("Steam")
                .join("compatibilitytools.d"),
        );
    }

    // The main library, then every additional library Steam records.
    for steam in &steam_roots {
        push(steam.join("steamapps").join("common"));
    }
    for steam in &steam_roots {
        for library in library_paths(steam) {
            push(library.join("compatibilitytools.d"));
            push(library.join("steamapps").join("common"));
        }
    }
    roots
}

/// The real path behind a discovered location, or the path itself when it
/// cannot be resolved (a root that does not exist yet, an unreadable
/// parent). Never fails — discovery is read-only and best-effort, and a
/// path that cannot be canonicalized is simply not a duplicate of
/// anything we can prove it is.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The library roots Steam records for one Steam installation, read from
/// its `libraryfolders.vdf` in the order the file lists them (Steam's own
/// priority order, #53).
///
/// Two locations, because Steam has used both: `<steam>/steamapps/` is
/// where it lives today, and `<steam>/config/` is where older releases
/// wrote it — and where Steam itself is reported to re-read the library
/// set from, regenerating `steamapps/` on start. Both are read; a library
/// named twice collapses through the canonical-root dedupe.
///
/// Steam has written both `KeyValues` shapes for this file and Cellar reads
/// either, tolerantly: the modern `"libraryfolders" { "0" { "path" "…" } }`
/// set and the legacy index set `"libraryfolders" { "0" "…" }`. A missing,
/// unreadable, or malformed file contributes nothing — the fixed roots
/// stand on their own, and discovery never errors or panics on one.
fn library_paths(steam: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for vdf in [
        steam.join("steamapps").join("libraryfolders.vdf"),
        steam.join("config").join("libraryfolders.vdf"),
    ] {
        // Read-only: opened for reading, never created.
        let Ok(body) = std::fs::read_to_string(&vdf) else {
            continue;
        };
        paths.extend(library_paths_from_vdf(&body));
    }
    paths
}

/// Parse the library roots out of a `libraryfolders.vdf` body. Hand-rolled
/// on purpose (#53): the file is a tiny subset of Valve's text `KeyValues` —
/// `"key" "value"` pairs and `"key" { … }` blocks — and a dependency would
/// buy a general VDF parser to read one line shape, in a provider crate
/// whose dependency budget is `core` alone.
///
/// Deliberately forgiving: it *looks for* the paths rather than validating
/// a document. Both shapes real Steam has written for this file are read —
/// the modern `"libraryfolders" { "0" { "path" "…" } }` and the legacy
/// index set `"libraryfolders" { "0" "…" }` — as is the `"paths"` nesting
/// some builds use. Anything else, including a truncated or corrupt file,
/// is skipped rather than reported: a bad vdf degrades to the fixed roots
/// and never errors or panics.
fn library_paths_from_vdf(body: &str) -> Vec<PathBuf> {
    let tokens = vdf_tokens(body);
    let mut cursor = 0;
    let document = vdf_blocks(&tokens, &mut cursor, 0);
    let Some(libraryfolders) = document
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("libraryfolders"))
    else {
        // No `libraryfolders` section: nothing recognizable, so nothing
        // extra to scan. The fixed roots still apply.
        return Vec::new();
    };
    let mut paths = Vec::new();
    collect_paths(&libraryfolders.1, &mut paths);
    paths
}

/// Whether a `KeyValues` key is one of Steam's numeric library indexes.
fn is_index(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit())
}

/// Take one entry's path out of a `libraryfolders` subtree, in file order.
/// The modern shape nests a block per index and reads its `"path"`; the
/// legacy shape maps an index straight to a string. A `"paths"` block is
/// entered the same way, so the extra nesting some Steam builds write is
/// read without a second rule.
fn collect_paths(node: &VdfNode, paths: &mut Vec<PathBuf>) {
    match node {
        // The legacy shape: the index maps straight to a path.
        VdfNode::Text(path) => push_path(paths, path),
        VdfNode::Block(entries) => {
            for (key, value) in entries {
                if is_index(key) || key.eq_ignore_ascii_case("paths") {
                    collect_paths(value, paths);
                } else if key.eq_ignore_ascii_case("path") {
                    if let VdfNode::Text(path) = value {
                        push_path(paths, path);
                    }
                }
                // Every other key (label, contentid, totalsize, the per-app
                // `apps` map) is metadata: skipped, never guessed at.
            }
        }
    }
}

/// Record one discovered library path. A blank value is skipped, and so is
/// a relative one: a Steam file must not be able to point discovery under
/// `$PWD` (the #61 rule, applied to host data).
fn push_path(paths: &mut Vec<PathBuf>, raw: &str) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        paths.push(path.to_path_buf());
    }
}

/// One node of a parsed text `KeyValues` document.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VdfNode {
    /// A string value.
    Text(String),
    /// A `{ … }` block, in file order.
    Block(Vec<(String, VdfNode)>),
}

/// One lexical token: a string (quoted or bare), or a brace.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VdfToken {
    Str(String),
    Open,
    Close,
}

/// Split a `KeyValues` body into tokens. Quotes are stripped and Valve's
/// `\"` / `\\` escapes honored; `//` line comments and braces are
/// recognized.
fn vdf_tokens(body: &str) -> Vec<VdfToken> {
    let mut tokens = Vec::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => tokens.push(VdfToken::Open),
            '}' => tokens.push(VdfToken::Close),
            '"' => tokens.push(VdfToken::Str(read_quoted(&mut chars))),
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => {}
            c => {
                // A bare token (Valve writes some values unquoted) runs
                // until whitespace or a structural character.
                let mut token = String::from(c);
                while let Some(next) = chars.peek() {
                    if next.is_whitespace() || *next == '"' || *next == '{' || *next == '}' {
                        break;
                    }
                    token.push(*next);
                    chars.next();
                }
                tokens.push(VdfToken::Str(token));
            }
        }
    }
    tokens
}

/// Parse `key value` pairs and `key { … }` blocks from `tokens`, advancing
/// `cursor`. Tolerant by construction: a stray brace is skipped, a block
/// left unclosed simply ends the parse, and whatever was read before the
/// damage is returned.
///
/// `depth` bounds the recursion: a hand-mangled file of nothing but
/// unbalanced openers would otherwise recurse per token, and a crash is the
/// one outcome discovery must never have. Past the bound the parse stops
/// (this file nests three deep in practice).
fn vdf_blocks(tokens: &[VdfToken], cursor: &mut usize, depth: usize) -> Vec<(String, VdfNode)> {
    const MAX_DEPTH: usize = 16;
    if depth >= MAX_DEPTH {
        *cursor = tokens.len();
        return Vec::new();
    }
    let mut entries = Vec::new();
    while let Some(token) = tokens.get(*cursor) {
        match token {
            VdfToken::Close => {
                *cursor += 1;
                break;
            }
            // A block opener with no key before it: skip the brace.
            VdfToken::Open => *cursor += 1,
            VdfToken::Str(key) => {
                let key = (*key).clone();
                *cursor += 1;
                match tokens.get(*cursor) {
                    Some(VdfToken::Open) => {
                        *cursor += 1;
                        let block = vdf_blocks(tokens, cursor, depth + 1);
                        entries.push((key, VdfNode::Block(block)));
                    }
                    Some(VdfToken::Str(value)) => {
                        *cursor += 1;
                        entries.push((key, VdfNode::Text((*value).clone())));
                    }
                    // A key with nothing after it: the file is truncated.
                    Some(VdfToken::Close) | None => break,
                }
            }
        }
    }
    entries
}

/// Read the rest of a quoted `KeyValues` string, handling Valve's `\"` and
/// `\\` escapes. A trailing lone backslash takes the value as read.
fn read_quoted(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut value = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some(escaped) => value.push(escaped),
                None => break,
            },
            c => value.push(c),
        }
    }
    value
}

/// `execvp`'s "found and executable" predicate (the wine provider's).
fn executable_file(path: &Path) -> bool {
    path.is_file() && is_executable(path)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A scratch data home rooted at a unique temp dir.
    fn home(tag: &str) -> PathBuf {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("cellar-proton-{tag}-{}-{seq}", std::process::id()))
    }

    fn write_file(path: &Path, body: &[u8], executable: bool) {
        use std::os::unix::fs::PermissionsExt;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| panic!("mkdir: {e}"));
        }
        std::fs::write(path, body).unwrap_or_else(|e| panic!("write: {e}"));
        if executable {
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).unwrap();
        }
    }

    /// A working proton install dir.
    fn proton_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join("runtime/proton").join(name);
        write_file(&dir.join("proton"), b"#!/bin/sh\nexit 0\n", true);
        dir
    }

    /// A steam compat dir with a proton install inside.
    fn steam_dir(root: &Path, name: &str) -> PathBuf {
        let dir = root.join("steam/compatibilitytools.d").join(name);
        write_file(&dir.join("proton"), b"#!/bin/sh\nexit 0\n", true);
        dir
    }

    /// A Steam-distributed Proton install under some library's
    /// `steamapps/common`.
    fn library_proton(library: &Path, name: &str) -> PathBuf {
        let dir = library.join("steamapps/common").join(name);
        write_file(&dir.join("proton"), b"#!/bin/sh\nexit 0\n", true);
        dir
    }

    /// Write a `libraryfolders.vdf` body verbatim under `steam`.
    fn write_libraryfolders(steam: &Path, body: &str) {
        let vdf = steam.join("steamapps/libraryfolders.vdf");
        write_file(&vdf, body.as_bytes(), false);
    }

    /// A home with an XDG steam install, the legacy `~/.steam/steam` one,
    /// and the flatpak roots — the four Steam spellings discovery reads.
    /// Returns `(xdg_data_home, home)` as the strings the root derivation
    /// takes (the injection seam: the workspace forbids `unsafe`, so
    /// edition-2024 `set_var` is out).
    fn steam_home(tag: &str) -> (String, String) {
        let home = home(tag);
        let xdg = home.join(".local/share");
        for steam in [
            xdg.join("Steam"),
            home.join(".steam/steam"),
            home.join(".var/app/com.valvesoftware.Steam/data/Steam"),
            home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        ] {
            std::fs::create_dir_all(steam.join("steamapps")).unwrap();
        }
        (
            xdg.to_string_lossy().into_owned(),
            home.to_string_lossy().into_owned(),
        )
    }

    /// The `libraryfolders.vdf` a Steam install with the given extra
    /// libraries writes — the modern nested shape real Steam uses.
    fn libraryfolders_body(libraries: &[PathBuf]) -> String {
        use std::fmt::Write as _;

        let mut body = String::from("\"libraryfolders\"\n{\n");
        for (index, library) in libraries.iter().enumerate() {
            // Unreachable to fail on: writing into a String.
            let _ = write!(
                body,
                "\t\"{index}\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t\t\"label\"\t\t\"\"\n\t}}\n",
                library.display()
            );
        }
        body.push_str("}\n");
        body
    }

    /// AC (#53): the class of miss this issue is about. A Proton sitting
    /// on a *second* Steam library — the multi-library case, not an edge
    /// case — is discovered by reading `libraryfolders.vdf`.
    #[test]
    fn a_second_librarys_proton_is_discovered_through_libraryfolders() {
        let (xdg, home) = steam_home("second-library");
        let secondary = PathBuf::from(&home).join("mnt/games/SteamLibrary");
        write_libraryfolders(
            &PathBuf::from(&xdg).join("Steam"),
            &libraryfolders_body(&[PathBuf::from(&xdg).join("Steam"), secondary.clone()]),
        );
        let wanted = library_proton(&secondary, "Proton - Hotfix");

        let scan = ProtonProvider::scan_steam(&steam_roots_with(Some(&xdg), Some(&home)));
        let dirs: Vec<&Path> = scan.iter().map(|proton| proton.dir.as_path()).collect();
        assert!(
            dirs.contains(&wanted.as_path()),
            "the secondary library is scanned: {dirs:?}"
        );
    }

    /// The legacy shape Steam wrote for years: the index maps straight to
    /// a path, with no `path` sub-key.
    #[test]
    fn the_legacy_libraryfolders_index_set_is_read() {
        let (xdg, home) = steam_home("legacy-index");
        let secondary = PathBuf::from(&home).join("SteamLibrary");
        let body = format!(
            "\"libraryfolders\"\n{{\n\t\"0\"\t\t\"{}\"\n\t\"1\"\t\t\"{}\"\n}}\n",
            PathBuf::from(&xdg).join("Steam").display(),
            secondary.display()
        );
        write_libraryfolders(&PathBuf::from(&xdg).join("Steam"), &body);
        let wanted = library_proton(&secondary, "Proton 8.0");

        let scan = ProtonProvider::scan_steam(&steam_roots_with(Some(&xdg), Some(&home)));
        assert!(
            scan.iter().any(|proton| proton.dir == wanted),
            "a legacy index entry is a library: {:?}",
            scan.iter().map(|p| &p.dir).collect::<Vec<_>>()
        );
    }

    /// AC (#53): a missing or malformed `libraryfolders.vdf` degrades to the
    /// fixed roots — no error, no panic, and the extra libraries simply
    /// contribute nothing.
    #[test]
    fn a_missing_or_malformed_libraryfolders_degrades_to_the_fixed_roots() {
        let (xdg, home) = steam_home("degrade");
        let steam = PathBuf::from(&xdg).join("Steam");
        let fixed = steam_roots_with(Some(&xdg), Some(&home));

        // Missing file.
        assert!(!steam.join("steamapps/libraryfolders.vdf").exists());
        assert_eq!(
            steam_roots_with(Some(&xdg), Some(&home)),
            fixed,
            "a missing vdf changes nothing"
        );

        // The failure protontricks documents as "corrupted", and a
        // half-written one: both must not panic and must not invent roots.
        for body in [
            "Corrupted",
            "\"libraryfolders\"\n{\n\t\"0\"\n",
            "{\"0\": {\"path\": ",
        ] {
            write_libraryfolders(&steam, body);
            assert_eq!(
                steam_roots_with(Some(&xdg), Some(&home)),
                fixed,
                "a malformed vdf degrades to the fixed roots: {body:?}"
            );
        }
    }

    /// AC (#53): the flatpak Steam roots are part of the floor. Both
    /// spellings flatpak Steam installs use are scanned.
    #[test]
    fn the_flatpak_compatibility_roots_are_searched() {
        let (xdg, home) = steam_home("flatpak");
        let flatpak = PathBuf::from(&home).join(".var/app/com.valvesoftware.Steam");
        let roots = steam_roots_with(Some(&xdg), Some(&home));
        for steam in [
            flatpak.join("data/Steam"),
            flatpak.join(".local/share/Steam"),
        ] {
            assert!(
                roots.contains(&steam.join("compatibilitytools.d")),
                "the flatpak compat-tools root is scanned: {}",
                steam.display()
            );
        }
        // And an install there is discovered, not merely enumerated.
        let ge = flatpak.join("data/Steam/compatibilitytools.d/GE-Proton10-4");
        write_file(&ge.join("proton"), b"#!/bin/sh\nexit 0\n", true);
        let scan = ProtonProvider::scan_steam(&roots);
        assert!(
            scan.iter().any(|proton| proton.dir == ge),
            "flatpak Steam's compat tools are discovered: {:?}",
            scan.iter().map(|p| &p.dir).collect::<Vec<_>>()
        );
    }

    /// AC (#53): `~/.steam/steam` routinely aliases the XDG install, so the
    /// same Proton is reachable through two roots. Canonicalize-then-dedupe
    /// means one row, not two.
    #[test]
    fn overlapping_symlinked_roots_do_not_duplicate_an_install() {
        let (xdg, home) = steam_home("overlap");
        // The real install, and the symlink Valve's own docs spell.
        let real = PathBuf::from(&xdg).join("Steam/compatibilitytools.d/GE-Proton11-5");
        write_file(&real.join("proton"), b"#!/bin/sh\nexit 0\n", true);
        let link = PathBuf::from(&home).join(".steam/steam");
        let _ = std::fs::remove_dir_all(&link);
        std::os::unix::fs::symlink(PathBuf::from(&xdg).join("Steam"), link).unwrap();

        let scan = ProtonProvider::scan_steam(&steam_roots_with(Some(&xdg), Some(&home)));
        let hits = scan
            .iter()
            .filter(|proton| proton.dir.ends_with("GE-Proton11-5"))
            .count();
        assert_eq!(hits, 1, "one install, one row: {scan:?}");
    }

    /// AC (#53): the stated order. Compat-tools roots before library
    /// `common/` roots, and the multi-library order is Steam's own
    /// `libraryfolders.vdf` order — so resolution is not host-dependent.
    #[test]
    fn discovery_order_is_the_stated_rule() {
        let (xdg, home) = steam_home("order");
        let first = PathBuf::from(&home).join("library-first");
        let second = PathBuf::from(&home).join("library-second");
        write_libraryfolders(
            &PathBuf::from(&xdg).join("Steam"),
            &libraryfolders_body(&[
                PathBuf::from(&xdg).join("Steam"),
                first.clone(),
                second.clone(),
            ]),
        );
        // One Proton per class, named so sorting inside a root is
        // observable too.
        library_proton(&second, "Proton 9.0");
        library_proton(&first, "Proton 8.0");
        let xdg_steam = PathBuf::from(&xdg).join("Steam");
        write_file(
            &xdg_steam.join("compatibilitytools.d/zzz-compat/proton"),
            b"#!/bin/sh\nexit 0\n",
            true,
        );

        let scan = ProtonProvider::scan_steam(&steam_roots_with(Some(&xdg), Some(&home)));
        let order: Vec<&str> = scan.iter().map(|proton| proton.version.as_str()).collect();
        assert_eq!(
            order,
            [
                "zzz-compat", // compatibilitytools.d roots come first,
                "Proton 8.0", // then libraries in Steam's own order,
                "Proton 9.0", // name-sorted within a root.
            ],
            "the discovery order is the documented rule, not the host's"
        );
    }

    /// AC (#53): an entry with no executable `proton` script is not a
    /// runner — in a secondary library exactly as in the main one.
    #[test]
    fn a_library_entry_without_an_executable_proton_is_not_a_runner() {
        let (xdg, home) = steam_home("not-a-runner");
        let secondary = PathBuf::from(&home).join("SteamLibrary");
        write_libraryfolders(
            &PathBuf::from(&xdg).join("Steam"),
            &libraryfolders_body(std::slice::from_ref(&secondary)),
        );
        // A Proton-looking directory whose `proton` is a non-executable
        // file, and one with no `proton` at all.
        write_file(
            &secondary.join("steamapps/common/Proton 9.0/proton"),
            b"text",
            false,
        );
        write_file(
            &secondary.join("steamapps/common/Proton 7.0/version"),
            b"7.0",
            false,
        );

        let scan = ProtonProvider::scan_steam(&steam_roots_with(Some(&xdg), Some(&home)));
        assert!(
            scan.is_empty(),
            "neither directory is a working install: {:?}",
            scan.iter().map(|p| &p.dir).collect::<Vec<_>>()
        );
    }

    /// The launch failure for an unresolvable Proton stops saying "install
    /// it" when the roots were read and held nothing — it names what was
    /// read instead (#53).
    /// A realistic file, warts and all: a comment, Valve's `\"` escape, the
    /// per-app `apps` metadata map, a blank path, and a relative one (which
    /// must be skipped — the #61 rule applied to host data, never a
    /// `$PWD`-relative discovery root).
    #[test]
    fn a_realistic_libraryfolders_body_yields_its_paths_in_order() {
        let body = concat!(
            "\"libraryfolders\"\n{\n",
            "\t// written by Steam\r\n",
            "\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/home/me/.local/share/Steam\"\n",
            "\t\t\"label\"\t\t\"\"\n\t\t\"contentid\"\t\t\"1234567890123456789\"\n",
            "\t\t\"apps\"\n\t\t{\n\t\t\t\"228980\"\t\t\"7654321\"\n\t\t}\n\t}\n",
            "\t\"1\"\n\t{\n\t\t\"path\"\t\t\"/mnt/games/Steam \\\"fast\\\"\"\n\t}\n",
            "\t\"2\"\n\t{\n\t\t\"path\"\t\t\"  \"\n\t}\n",
            "\t\"3\"\n\t{\n\t\t\"path\"\t\t\"SteamLibrary\"\n\t}\n",
            "}\n",
        );
        assert_eq!(
            library_paths_from_vdf(body),
            vec![
                PathBuf::from("/home/me/.local/share/Steam"),
                // Valve's escaping honored: the value is a path with a quote
                // in it, not a path that ends at the escape.
                PathBuf::from("/mnt/games/Steam \"fast\""),
            ],
            "in file order, skipping blank and relative entries"
        );
    }

    /// A hand-mangled file — nothing but unbalanced openers — must not
    /// recurse per token: discovery is best-effort and never a crash.
    #[test]
    fn a_pathological_libraryfolders_body_terminates() {
        let body = format!(
            "\"libraryfolders\"\n{{\n{}{}",
            "\t\"0\"\n\t{\n".repeat(5_000),
            "\"unterminated\n"
        );
        assert!(library_paths_from_vdf(&body).is_empty());
    }

    /// The extra nesting some Steam builds write — a `paths` object under
    /// `libraryfolders` — is read by the same rule as the index set.
    #[test]
    fn the_paths_nesting_is_read() {
        let body = concat!(
            "\"libraryfolders\"\n{\n\t\"paths\"\n\t{\n",
            "\t\t\"0\"\t\t\"/mnt/a\"\n",
            "\t\t\"1\"\t\t{\n\t\t\t\"path\"\t\t\"/mnt/b\"\n\t\t}\n",
            "\t}\n}\n",
        );
        assert_eq!(
            library_paths_from_vdf(body),
            vec![PathBuf::from("/mnt/a"), PathBuf::from("/mnt/b")]
        );
    }

    /// Older Steam releases wrote `libraryfolders.vdf` under `config/` rather
    /// than `steamapps/`; both are read, so a library is found wherever the
    /// install on disk keeps its record.
    #[test]
    fn a_config_lettered_libraryfolders_is_read_too() {
        let (xdg, home) = steam_home("config-vdf");
        let secondary = PathBuf::from(&home).join("old-style-library");
        write_file(
            &PathBuf::from(&xdg).join("Steam/config/libraryfolders.vdf"),
            libraryfolders_body(std::slice::from_ref(&secondary)).as_bytes(),
            false,
        );
        let wanted = library_proton(&secondary, "Proton 5.0");

        let scan = ProtonProvider::scan_steam(&steam_roots_with(Some(&xdg), Some(&home)));
        assert!(
            scan.iter().any(|proton| proton.dir == wanted),
            "the older config/ location is read: {:?}",
            scan.iter().map(|p| &p.dir).collect::<Vec<_>>()
        );
    }

    /// #61, applied to discovery: a misconfigured environment must yield
    /// *fewer* roots, never a `$PWD`-relative one.
    #[test]
    fn a_misconfigured_environment_yields_fewer_roots_never_relative_ones() {
        let roots = steam_roots_with(Some("relative/data"), Some("/home/me"));
        assert!(
            !roots.is_empty(),
            "the usable half of the environment still contributes"
        );
        assert!(
            roots.iter().all(|root| root.is_absolute()),
            "nothing may point at $PWD: {roots:?}"
        );
        assert!(
            !roots.iter().any(|root| root.starts_with("relative")),
            "the relative XDG value contributes nothing: {roots:?}"
        );
        assert!(
            roots.contains(&PathBuf::from("/home/me/.steam/steam/compatibilitytools.d")),
            "the $HOME-derived roots survive a broken XDG value: {roots:?}"
        );

        // A relative HOME taints nothing that does not use it: the XDG data
        // home alone still yields its Steam roots.
        let roots = steam_roots_with(Some("/data"), Some("relative/home"));
        assert!(
            roots.contains(&PathBuf::from("/data/Steam/compatibilitytools.d")),
            "the XDG roots stand: {roots:?}"
        );
        assert!(
            roots.iter().all(|root| root.is_absolute()),
            "the relative HOME invents nothing: {roots:?}"
        );
    }

    #[test]
    fn an_exhausted_steam_stage_names_the_roots_it_read() {
        let root = home("searched-nothing");
        let steam_root = root.join("steam/steamapps/common");
        write_file(
            &steam_root.join("common/Steamworks Common Redistributables/x"),
            b"x",
            false,
        );
        let provider = ProtonProvider::with_roots(root.join("runtime"), vec![steam_root.clone()]);
        assert_eq!(
            provider.resolve(&RunnerSpec::new(RunnerFamily::Proton)),
            Err(ResolveError::Unresolvable {
                family: RunnerFamily::Proton,
                cause: UnresolvedCause::SearchedNothing {
                    searched: vec![steam_root],
                },
            })
        );
    }

    #[test]
    fn a_configured_path_wins() {
        let root = home("configured");
        let configured = proton_dir(&root, "GE-Proton9-1");
        let spec = RunnerSpec::with_configured(
            RunnerFamily::Proton,
            ConfiguredRunner::Path(configured.clone()),
        );
        let provider = ProtonProvider::with_roots(root.join("runtime"), vec![]);
        let resolved = provider.resolve(&spec).expect("resolve");
        let RunnerInstall::Discovered { path, version } = &resolved.reference.install else {
            panic!("configured paths are discover-only");
        };
        assert_eq!(path, &configured);
        assert_eq!(version, &None);
    }

    #[test]
    fn a_broken_configured_path_falls_through_to_managed() {
        let root = home("broken-config");
        let broken = root.join("configured");
        write_file(&broken.join("proton"), b"plain text", false);
        proton_dir(&root, "GE-Proton11-5");
        let spec =
            RunnerSpec::with_configured(RunnerFamily::Proton, ConfiguredRunner::Path(broken));
        let provider = ProtonProvider::with_roots(root.join("runtime"), vec![]);
        let resolved = provider.resolve(&spec).expect("falls through");
        assert_eq!(resolved.mode, ProviderMode::Managed);
        let RunnerInstall::Managed { path, version } = &resolved.reference.install else {
            panic!("managed");
        };
        assert_eq!(path, &root.join("runtime/proton/GE-Proton11-5"));
        assert_eq!(version.as_str(), "GE-Proton11-5");
    }

    #[test]
    fn a_version_pin_resolves_the_exact_managed_install() {
        let root = home("pin");
        proton_dir(&root, "GE-Proton9-1");
        proton_dir(&root, "GE-Proton11-5");
        let spec = RunnerSpec::with_configured(
            RunnerFamily::Proton,
            ConfiguredRunner::Version("GE-Proton9-1".to_owned()),
        );
        let provider = ProtonProvider::with_roots(root.join("runtime"), vec![]);
        let resolved = provider.resolve(&spec).expect("pinned");
        assert_eq!(resolved.mode, ProviderMode::Managed);
        let RunnerInstall::Managed { path, version } = &resolved.reference.install else {
            panic!("managed");
        };
        assert_eq!(path, &root.join("runtime/proton/GE-Proton9-1"));
        assert_eq!(version.as_str(), "GE-Proton9-1");
    }

    #[test]
    fn a_missing_version_pin_is_not_installed_not_unresolvable() {
        let root = home("pin-missing");
        let spec = RunnerSpec::with_configured(
            RunnerFamily::Proton,
            ConfiguredRunner::Version("GE-Proton9-1".to_owned()),
        );
        let provider = ProtonProvider::with_roots(root.join("runtime"), vec![]);
        assert_eq!(
            provider.resolve(&spec).expect_err("not installed"),
            ResolveError::NotInstalled {
                family: RunnerFamily::Proton
            },
            "a pin never falls through to discovery — SuggestInstall names the version"
        );
    }

    #[test]
    fn without_a_pin_the_newest_managed_install_wins() {
        let root = home("newest");
        let old = proton_dir(&root, "GE-Proton9-1");
        let new = proton_dir(&root, "GE-Proton11-5");
        // Explicit distinct mtimes make "newest" deterministic (install
        // order is what the mtime records).
        let set_mtime = |path: &Path, seconds: u64| {
            let times = std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds));
            std::fs::File::options()
                .write(true)
                .open(path)
                .and_then(|file| file.set_times(times))
                .unwrap();
        };
        set_mtime(&new.join("proton"), 2_000_000_000);
        set_mtime(&old.join("proton"), 1_000_000_000);
        let provider = ProtonProvider::with_roots(root.join("runtime"), vec![]);
        let resolved = provider
            .resolve(&RunnerSpec::new(RunnerFamily::Proton))
            .expect("newest resolves");
        assert_eq!(resolved.mode, ProviderMode::Managed);
        let RunnerInstall::Managed { path, version } = &resolved.reference.install else {
            panic!("managed");
        };
        assert_eq!(path, &new, "the newer install wins");
        assert_eq!(version.as_str(), "GE-Proton11-5");
    }

    #[test]
    fn manage_installs_land_on_top_of_steam_discovery() {
        let root = home("managed-over-steam");
        steam_dir(&root, "GE-Proton8-20");
        proton_dir(&root, "GE-Proton11-5");
        let steam_roots = vec![root.join("steam/compatibilitytools.d")];
        let provider = ProtonProvider::with_roots(root.join("runtime"), steam_roots);
        let resolved = provider
            .resolve(&RunnerSpec::new(RunnerFamily::Proton))
            .expect("managed wins");
        assert_eq!(resolved.mode, ProviderMode::Managed);
        let RunnerInstall::Managed { path, .. } = &resolved.reference.install else {
            panic!("managed");
        };
        assert_eq!(
            path,
            &root.join("runtime/proton/GE-Proton11-5"),
            "the resolution order is configured → managed → steam"
        );
    }

    #[test]
    fn steam_discovery_is_the_read_only_last_resort() {
        let root = home("steam-only");
        steam_dir(&root, "GE-Proton8-20");
        let steam_roots = vec![root.join("steam/compatibilitytools.d")];
        let provider = ProtonProvider::with_roots(root.join("runtime"), steam_roots);
        let resolved = provider
            .resolve(&RunnerSpec::new(RunnerFamily::Proton))
            .expect("steam discovery");
        assert_eq!(resolved.mode, ProviderMode::DiscoverOnly);
        let RunnerInstall::Discovered { path, version } = &resolved.reference.install else {
            panic!("discovered");
        };
        assert_eq!(path, &root.join("steam/compatibilitytools.d/GE-Proton8-20"));
        assert_eq!(version.as_deref(), Some("GE-Proton8-20"));
    }

    #[test]
    fn steam_scan_is_deterministic_and_read_only() {
        let root = home("scan");
        steam_dir(&root, "beta");
        steam_dir(&root, "GE-Proton11-5");
        steam_dir(&root, "alpha");
        // A non-proton dir is skipped.
        write_file(
            &root.join("steam/compatibilitytools.d/not-proton/garbage"),
            b"x",
            false,
        );
        let scan = ProtonProvider::scan_steam(&[root.join("steam/compatibilitytools.d")]);
        let versions: Vec<&str> = scan.iter().map(|proton| proton.version.as_str()).collect();
        assert_eq!(
            versions,
            ["GE-Proton11-5", "alpha", "beta"],
            "name-sorted, non-Proton dirs skipped"
        );
    }

    #[test]
    fn steam_common_proton_dirs_are_discovered() {
        let root = home("common");
        let dir = root.join("steam/steamapps/common/Proton 9.0");
        write_file(&dir.join("proton"), b"#!/bin/sh\nexit 0\n", true);
        let scan = ProtonProvider::scan_steam(&[root.join("steam/steamapps/common")]);
        assert_eq!(scan.len(), 1);
        assert_eq!(scan[0].version, "Proton 9.0");
    }

    #[test]
    fn other_families_are_not_serviced() {
        let provider = ProtonProvider::with_roots(PathBuf::from("/nope"), vec![]);
        let err = provider
            .resolve(&RunnerSpec::new(RunnerFamily::Wine))
            .expect_err("proton does not service wine");
        assert_eq!(err.family(), RunnerFamily::Proton);
    }

    #[test]
    fn nothing_at_all_is_unresolvable_with_a_suggestion() {
        let root = home("nothing");
        let provider = ProtonProvider::with_roots(root.join("runtime"), vec![]);
        assert_eq!(
            provider.resolve(&RunnerSpec::new(RunnerFamily::Proton)),
            Err(ResolveError::Unresolvable {
                family: RunnerFamily::Proton,
                cause: UnresolvedCause::NoneFound {
                    mode: ProviderMode::Managed,
                },
            })
        );
    }
}
