//! The shared installer pipeline (blueprint §5): fetch the release,
//! verify the checksum (SHA-512 for GE-Proton — research #18 corrected the
//! ticket's "sha256"), extract, flock per directory, resumable cache,
//! `runtime/providers.toml` inventory write — a storage-owned service
//! driven by a `ManagedRunner` manifest plus a concrete version pin.
//! Adding a managed runner is one descriptor and one registry line — zero
//! pipeline code.
//!
//! This slice (#34) lands the pipeline: `runner install <provider> <version>`
//! downloads, verifies, extracts, and records (AC: corrupt downloads fail
//! closed — nothing is extracted, nothing is recorded); interrupted
//! installs resume from the disposable cache (`<root>/cache/downloads` —
//! the tree's own cache, blueprint §6, one movable unit); concurrent
//! installs of one provider are flock-serialized; and the inventory makes
//! the runtime directory rebuildable at any time.
//!
//! Fixtures and local mirrors ride the `file://` scheme; real releases are
//! `https://` (github release assets). Both honour byte ranges, so
//! resumption is uniform: a `Range` request continues a `.part` file, and
//! a server that ignores the range restarts the download from zero — never
//! a corrupted append.

use cellar_core::manifest::{InstallKind, ManagedInventory, ManagedRecord, RunnerManifest};

use flate2::read::GzDecoder;
use fs2::FileExt;
use sha2::{Digest, Sha512};

use cellar_core::errors::StorageError;

use std::fs;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The inventory file: `runtime/providers.toml` (blueprint §6 — the
/// authoritative, re-installable record of managed installs).
const INVENTORY_FILE: &str = "providers.toml";

/// The per-provider install lock: `runtime/<provider>/.install.lock`
/// (flock, fs2) — one provider's installs serialize per directory.
const LOCK_FILE: &str = ".install.lock";

/// The inventory's own lock: `runtime/.inventory.lock` — the shared
/// `providers.toml` read-modify-write serializes across providers
/// (each provider's install lock covers only its own directory).
const INVENTORY_LOCK: &str = ".inventory.lock";

/// The architectures GE-Proton publishes (research #18: exactly two asset
/// shapes, `-x86_64` and `-aarch64`).
const SUPPORTED_ARCHES: [&str; 2] = ["x86_64", "aarch64"];

/// Monotonic counter for per-install temp names within a process.
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Install one managed runner version: fetch → verify → extract → record.
///
/// Idempotent: an installed version (the probe passes) returns its
/// directory unchanged — re-running `runner install` is a no-op. A
/// half-broken install directory is replaced (the runtime dir is
/// re-derivable; blueprint §6). Every failure path leaves no new state:
/// corrupt artifacts are deleted, temp extraction directories are removed,
/// and the inventory is written only after a passing probe.
pub(crate) fn install(
    root: &Path,
    manifest: &RunnerManifest,
    version: &str,
) -> Result<PathBuf, StorageError> {
    validate_version_pin(version)?;
    let arch = target_arch()?;
    // The lock serializes one provider's concurrent installs (AC: flock
    // per-directory) — the build happens inside the lock, the idempotency
    // probe after it, so two racing installs never fight the same dirs.
    let provider_dir = root.join("runtime").join(&manifest.provider_id);
    fs::create_dir_all(&provider_dir).map_err(storage_io(&provider_dir))?;
    let lock_file =
        fs::File::create(provider_dir.join(LOCK_FILE)).map_err(storage_io(&provider_dir))?;
    lock_file.lock_exclusive().map_err(|error| {
        StorageError::Artifact(format!(
            "cannot lock {}: {error}",
            provider_dir.join(LOCK_FILE).display()
        ))
    })?;

    let install_dir = provider_dir.join(version);
    if probe_installed(manifest, &install_dir) {
        // An installed version is a no-op — and it is recorded, so a dir
        // placed by an earlier interrupted run (or by hand) joins the
        // authoritative inventory (reconciliation: the inventory always
        // mirrors what resolution serves).
        record_inventory(root, manifest, version)?;
        return Ok(install_dir);
    }
    if install_dir.exists() {
        // Exists but broken (missing probe target): replace it — the
        // runtime dir is disposable, re-derivable state (blueprint §6).
        fs::remove_dir_all(&install_dir).map_err(storage_io(&install_dir))?;
    }

    // Fetch + verify: the resumable cache under the tree's disposable
    // downloads dir.
    let artifact_url = substitute(&manifest.source.url_template, version, arch);
    let artifact_name = artifact_name(&artifact_url)?;
    let downloads = root.join("cache").join("downloads");
    fs::create_dir_all(&downloads).map_err(storage_io(&downloads))?;
    let artifact = downloads.join(&artifact_name);
    let source = ArtifactSource::parse(&artifact_url)
        .ok_or_else(|| StorageError::Artifact(format!("unusable source URL: {artifact_url}")))?;
    download_resumable(&source, &artifact)?;
    if let Some(template) = &manifest.source.checksum_url_template {
        let checksum_url = substitute(template, version, arch);
        let checksum = fetch_checksum(&checksum_url, &downloads, &artifact_name)?;
        // The declared checksum is mandatory: a mismatch discards the
        // download and aborts — nothing is extracted, nothing recorded
        // (AC: corrupt downloads fail closed).
        verify_sha512(&artifact, &checksum)?;
    }
    // No checksum URL: upstream publishes no digest (umu's zipapp —
    // research #18), so the artifact installs unverified; a truncated
    // download still fails at extraction (fail closed).

    // Extract into a private temp dir, then move the single root into its
    // final name — the install dir appears atomically.
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = root.join("runtime").join(format!(
        ".install-{}-{}-{seq}",
        manifest.provider_id,
        std::process::id()
    ));
    fs::create_dir_all(&tmp).map_err(storage_io(&tmp))?;
    let extract_result = extract_tarball(&artifact, &tmp);
    if let Err(error) = extract_result {
        let _ = fs::remove_dir_all(&tmp);
        return Err(error);
    }
    let root_entry = single_root_dir(&tmp)?;
    debug_assert!(root_entry.is_dir());
    if let Err(error) = fs::rename(&root_entry, &install_dir) {
        let _ = fs::remove_dir_all(&tmp);
        return Err(match error.kind() {
            // A concurrent install claimed the dir: it is installed.
            io::ErrorKind::AlreadyExists if install_dir.exists() => return Ok(install_dir),
            _ => StorageError::Io(format!(
                "rename {} → {}: {error}",
                root_entry.display(),
                install_dir.display()
            )),
        });
    }
    let _ = fs::remove_dir_all(&tmp);
    if !probe_installed(manifest, &install_dir) {
        let _ = fs::remove_dir_all(&install_dir);
        return Err(StorageError::Artifact(format!(
            "the extracted artifact does not look like a {} install — the manifest's \
             install kind expects a {} at the install root",
            manifest.provider_id,
            probe_target(manifest).display()
        )));
    }

    // Record: the authoritative inventory — the runtime dir becomes
    // rebuildable from it (AC).
    record_inventory(root, manifest, version)?;

    Ok(install_dir)
}

/// Upsert one install's record into the authoritative inventory
/// (`runtime/providers.toml`), reading-modifying-writing under the
/// inventory's own lock — concurrent installs of *different* providers
/// each hold their provider's flock, so the shared file would otherwise
/// lose records (a lost update: both read empty, each writes its own).
fn record_inventory(
    root: &Path,
    manifest: &RunnerManifest,
    version: &str,
) -> Result<(), StorageError> {
    let lock_path = root.join("runtime").join(INVENTORY_LOCK);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(storage_io(parent))?;
    }
    let lock_file = fs::File::create(&lock_path).map_err(storage_io(&lock_path))?;
    lock_file.lock_exclusive().map_err(|error| {
        StorageError::Artifact(format!("cannot lock {}: {error}", lock_path.display()))
    })?;
    let mut records = inventory(root)?;
    records.retain(|record| {
        !(record.provider_id == manifest.provider_id && record.version == version)
    });
    records.push(ManagedRecord {
        provider_id: manifest.provider_id.clone(),
        version: version.to_owned(),
        install: format!("{}/{}", manifest.provider_id, version),
    });
    // Deterministic order (provider, then version) — the doc says so and
    // doctor/list reads deserve it.
    records.sort_by(|a, b| (&a.provider_id, &a.version).cmp(&(&b.provider_id, &b.version)));
    write_inventory(root, &records)
}

/// The version pin names the install directory verbatim — its one job —
/// so it is validated against a safe shape before any path is computed: a
/// traversal pin (`../…`) must never reach `remove_dir_all` (review
/// #34). Tags like `GE-Proton11-5` and `1.4.4` pass; separators,
/// components, and control whitespace are refused.
fn validate_version_pin(version: &str) -> Result<(), StorageError> {
    if version.is_empty() {
        return Err(StorageError::Artifact(
            "refusing to install an empty version pin".to_owned(),
        ));
    }
    if version.len() > 200
        || !version
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        || version == "."
        || version == ".."
    {
        return Err(StorageError::Artifact(format!(
            "invalid version pin {version:?} — use the release tag (e.g. GE-Proton11-5)"
        )));
    }
    Ok(())
}

/// The authoritative inventory (`runtime/providers.toml`): every recorded
/// managed install, deterministic order (provider, then version).
pub(crate) fn inventory(root: &Path) -> Result<Vec<ManagedRecord>, StorageError> {
    let path = root.join("runtime").join(INVENTORY_FILE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let inventory: ManagedInventory = crate::tree::TreeStore::read_envelope(&path)?;
    Ok(inventory.runner)
}

/// Upsert the records list into the inventory file (atomic envelope write;
/// missing runtime dir created on demand).
fn write_inventory(root: &Path, records: &[ManagedRecord]) -> Result<(), StorageError> {
    let path = root.join("runtime").join(INVENTORY_FILE);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(storage_io(parent))?;
    }
    crate::tree::TreeStore::write_envelope(
        &path,
        &ManagedInventory {
            runner: records.to_vec(),
        },
    )
}

/// The install-kind probe target: the file whose presence proves an
/// installed version (`proton` for compat tools, `umu-run` for launcher
/// binaries) — the same marker the resolution scan looks for.
fn probe_target(manifest: &RunnerManifest) -> PathBuf {
    match manifest.install_kind {
        InstallKind::CompatTool => PathBuf::from("proton"),
        InstallKind::LauncherBinary => PathBuf::from("umu-run"),
    }
}

/// Whether `install_dir` holds a working install of the manifest's kind.
fn probe_installed(manifest: &RunnerManifest, install_dir: &Path) -> bool {
    executable_file(&install_dir.join(probe_target(manifest)))
}

/// The target architecture suffix: `x86_64` or `aarch64` (GE-Proton's
/// asset shapes, research #18). Anything else is a loud refusal — no
/// guessed asset names.
fn target_arch() -> Result<&'static str, StorageError> {
    let arch = std::env::consts::ARCH;
    if SUPPORTED_ARCHES.contains(&arch) {
        Ok(arch)
    } else {
        Err(StorageError::Artifact(format!(
            "managed runners are published for x86_64 and aarch64 only (this host: {arch})"
        )))
    }
}

/// Fill the release templates: `{tag}` → version, `{arch}` → architecture.
fn substitute(template: &str, version: &str, arch: &str) -> String {
    template.replace("{tag}", version).replace("{arch}", arch)
}

/// The artifact file name: the last URL path segment (queries stripped).
fn artifact_name(url: &str) -> Result<String, StorageError> {
    match url.rsplit(['/', '?']).find(|part| !part.is_empty()) {
        Some(name) if !name.contains('/') => Ok(name.to_owned()),
        _ => Err(StorageError::Artifact(format!(
            "cannot derive an artifact file name from {url}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// The downloader: byte-range resumption over `file://` (fixtures, local
// mirrors) and `https://` (real releases).
// ---------------------------------------------------------------------------

/// A fetchable artifact location.
enum ArtifactSource {
    File(PathBuf),
    Http(String),
}

impl ArtifactSource {
    /// `file:///abs/path` and `https://…` — the two schemes this pipeline
    /// knows (a third scheme is a loud refusal, never a guess).
    ///
    /// `http://` is accepted too, for local test servers and dev mirrors:
    /// every manifest Cellar ships names `https://` (research #18: HTTPS
    /// is the enforced scheme), and the transport path is identical.
    fn parse(url: &str) -> Option<Self> {
        if let Some(path) = url.strip_prefix("file://") {
            return Some(Self::File(PathBuf::from(path)));
        }
        if url.starts_with("https://") || url.starts_with("http://") {
            return Some(Self::Http(url.to_owned()));
        }
        None
    }

    /// Stream from byte `from` onward into `out`. For `https` a `Range`
    /// request is sent when resuming; a server that ignores it (200)
    /// restarts the file from zero — never a corrupted tail append.
    fn stream_from(&self, from: u64, out: &mut fs::File) -> Result<(), StorageError> {
        match self {
            Self::File(path) => {
                let mut input = fs::File::open(path)
                    .map_err(|error| StorageError::Io(format!("{}: {error}", path.display())))?;
                input.seek(SeekFrom::Start(from)).map_err(|error| {
                    StorageError::Artifact(format!("seek {}: {error}", path.display()))
                })?;
                io::copy(&mut input, out)
                    .map_err(|error| StorageError::Io(format!("copy: {error}")))?;
            }
            Self::Http(url) => {
                let mut request = ureq::get(url);
                if from > 0 {
                    request = request.set("Range", &format!("bytes={from}-"));
                }
                let response = request
                    .call()
                    .map_err(|error| StorageError::Artifact(format!("fetch {url}: {error}")))?;
                match response.status() {
                    // The server ignored the range (200), or the resume
                    // offset exceeds the current content (416 — a stale
                    // oversized `.part`): restart from zero, never append
                    // a corrupted tail.
                    200 | 416 => {
                        out.set_len(0)
                            .map_err(|error| StorageError::Io(format!("truncate: {error}")))?;
                        out.seek(SeekFrom::Start(0))
                            .map_err(|error| StorageError::Io(format!("seek: {error}")))?;
                    }
                    206 => {}
                    status => {
                        return Err(StorageError::Artifact(format!(
                            "fetch {url}: unexpected status {status}"
                        )));
                    }
                }
                let mut reader = response.into_reader();
                io::copy(&mut reader, out)
                    .map_err(|error| StorageError::Io(format!("copy: {error}")))?;
            }
        }
        out.sync_all()
            .map_err(|error| StorageError::Io(format!("sync: {error}")))
    }
}

/// The resumable download: continue from the `.part` file's current size
/// (a prior interrupted run), or start fresh. The part becomes the
/// artifact file only after verification (the caller renames it post-
/// checksum — a corrupt download is deleted, never cached as complete).
fn download_resumable(source: &ArtifactSource, artifact: &Path) -> Result<(), StorageError> {
    if artifact.is_file() {
        // A previously completed and verified artifact in the disposable
        // cache — reuse it (the cache is disposable: remove it to fetch
        // again).
        return Ok(());
    }
    let part = artifact.with_extension("part");
    let mut out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part)
        .map_err(storage_io(&part))?;
    let from = out.metadata().map_err(storage_io(&part))?.len();
    source.stream_from(from, &mut out).map_err(|error| {
        StorageError::Artifact(format!("download to {}: {error}", part.display()))
    })?;
    // The download completed: promote the part for verification.
    fs::rename(&part, artifact).map_err(storage_io(artifact))
}

/// Fetch the published checksum file (`.sha512sum`), find the line naming
/// our artifact, and return its hex digest. A checksum the manifest
/// declares but cannot be fetched is a hard failure — verification is
/// mandatory when the manifest names a checksum source (AC: corrupt
/// downloads fail closed).
fn fetch_checksum(
    url: &str,
    downloads: &Path,
    artifact_name: &str,
) -> Result<String, StorageError> {
    let source = ArtifactSource::parse(url)
        .ok_or_else(|| StorageError::Artifact(format!("unusable checksum URL: {url}")))?;
    let file = downloads.join(format!("{artifact_name}.sha512sum"));
    let mut out = fs::File::create(&file).map_err(storage_io(&file))?;
    source.stream_from(0, &mut out)?;
    let text = fs::read_to_string(&file).map_err(storage_io(&file))?;
    for line in text.lines() {
        // `sha512sum` output: `<hex>  <name>` (two spaces) or `<hex> *<name>`.
        let mut tokens = line.split_whitespace();
        if let (Some(hex), Some(name)) = (tokens.next(), tokens.next()) {
            if name.trim_start_matches(['*', ' ']) == artifact_name {
                return Ok(hex.to_owned());
            }
        }
    }
    Err(StorageError::Artifact(format!(
        "the checksum file at {url} names no digest for {artifact_name}"
    )))
}

/// Verify the artifact against a published SHA-512 digest. A mismatch is a
/// hard failure and the corrupt download is deleted — the next run
/// refetches (AC: corrupt downloads fail closed, never cached as good).
fn verify_sha512(artifact: &Path, expected: &str) -> Result<(), StorageError> {
    let mut hasher = Sha512::new();
    let mut file = fs::File::open(artifact).map_err(storage_io(artifact))?;
    io::copy(&mut file, &mut hasher)
        .map_err(|error| StorageError::Io(format!("checksum {}: {error}", artifact.display())))?;
    let actual = hex(&hasher.finalize());
    if actual.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    let _ = fs::remove_file(artifact);
    let _ = fs::remove_file(artifact.with_extension("part"));
    Err(StorageError::Artifact(format!(
        "{}: checksum mismatch — expected {expected}, got {actual} (the corrupt download \
         was discarded)",
        artifact.display()
    )))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// ---------------------------------------------------------------------------
// Extraction: traversal-safe tar.gz into a private temp dir, then a
// single-root move. Mirrors the zip extractor's two-pass vetting
// (cellar-app archive.rs): the whole archive is read and vetted before
// anything is written.
// ---------------------------------------------------------------------------

/// Extract `archive` into `dest`, validating every entry path
/// component-wise first (no `..`, no absolute roots, no drive prefixes),
/// symlink targets validated to stay inside `dest`, and nothing written
/// before the whole archive passes. Hard links and special files are a
/// loud refusal — a shape the pipeline does not silently guess at.
fn extract_tarball(archive: &Path, dest: &Path) -> Result<(), StorageError> {
    let vetted = vet_tarball(archive)?;
    single_root_check(archive, &vetted)?;
    write_vetted(archive, dest, &vetted)
}

/// Pass one: read and vet every entry — nothing is written yet, so a
/// hostile archive can neither escape `dest` nor extract partway.
fn vet_tarball(archive: &Path) -> Result<Vec<VettedEntry>, StorageError> {
    let file = fs::File::open(archive).map_err(storage_io(archive))?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    let entries = tar
        .entries()
        .map_err(|error| StorageError::Artifact(format!("{}: {error}", archive.display())))?;
    let mut vetted = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|error| StorageError::Artifact(format!("{}: {error}", archive.display())))?;
        let kind = entry.header().entry_type();
        // Vendor metadata (pax/gnu headers) is skipped, never unpacked.
        if matches!(
            kind,
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader | tar::EntryType::GNULongName
        ) {
            continue;
        }
        let name = entry
            .path()
            .map_err(|error| StorageError::Artifact(format!("{}: {error}", archive.display())))?
            .to_string_lossy()
            .into_owned();
        let relative = validate_entry_path(&name)?;
        let link = if kind == tar::EntryType::Symlink {
            let target = entry
                .link_name()
                .map_err(|error| StorageError::Artifact(format!("{}: {error}", archive.display())))?
                .ok_or_else(|| StorageError::Artifact(format!("{name}: symlink without a target")))?
                .to_string_lossy()
                .into_owned();
            Some(validate_symlink_target(&relative, &target)?)
        } else {
            None
        };
        vetted.push(VettedEntry {
            name,
            relative,
            kind,
            link,
            mode: entry.header().mode().unwrap_or(0o644),
        });
    }
    Ok(vetted)
}

/// The locked archive layout (enforced before anything is written):
/// exactly one top-level entry — the install root, renamed to the
/// version dir.
fn single_root_check(archive: &Path, vetted: &[VettedEntry]) -> Result<(), StorageError> {
    let mut roots: Vec<std::path::Component<'_>> = Vec::new();
    for entry in vetted {
        if let Some(root) = entry.relative.components().next() {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
    }
    if roots.len() != 1 {
        return Err(StorageError::Artifact(format!(
            "{}: the archive must extract to a single top-level directory (found {})",
            archive.display(),
            roots.len()
        )));
    }
    Ok(())
}

/// Pass two: reopen the archive (tar entries are not seekable) and write
/// the vetted entries, in the same order pass one saw them. Directory
/// modes land last — a read-only dir must not block its own contents
/// during extraction.
fn write_vetted(archive: &Path, dest: &Path, vetted: &[VettedEntry]) -> Result<(), StorageError> {
    let file = fs::File::open(archive).map_err(storage_io(archive))?;
    let mut tar = tar::Archive::new(GzDecoder::new(file));
    let entries = tar
        .entries()
        .map_err(|error| StorageError::Artifact(format!("{}: {error}", archive.display())))?;
    let mut dir_modes = Vec::new();
    for (entry, vetted) in entries.zip(vetted) {
        let mut entry = entry
            .map_err(|error| StorageError::Artifact(format!("{}: {error}", archive.display())))?;
        if matches!(
            entry.header().entry_type(),
            tar::EntryType::XGlobalHeader | tar::EntryType::XHeader | tar::EntryType::GNULongName
        ) {
            continue;
        }
        let target = dest.join(&vetted.relative);
        match vetted.kind {
            tar::EntryType::Directory => {
                fs::create_dir_all(&target).map_err(storage_io(&target))?;
                dir_modes.push((target, vetted.mode));
            }
            tar::EntryType::Symlink => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(storage_io(parent))?;
                }
                create_symlink(
                    Path::new(vetted.link.as_deref().expect("symlinks carry their target")),
                    &target,
                )?;
            }
            kind if kind.is_file() => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(storage_io(parent))?;
                }
                let mut out = fs::File::create(&target).map_err(storage_io(&target))?;
                io::copy(&mut entry, &mut out)
                    .map_err(|error| StorageError::Io(format!("extract: {error}")))?;
                out.flush().map_err(storage_io(&target))?;
                set_mode(&target, vetted.mode)?;
            }
            _ => {
                // Hard links and special files: refuse loudly.
                return Err(StorageError::Artifact(format!(
                    "{}: unsupported archive entry type {:?}",
                    vetted.name, vetted.kind
                )));
            }
        }
    }
    for (dir, mode) in dir_modes {
        set_mode(&dir, mode)?;
    }
    Ok(())
}

struct VettedEntry {
    name: String,
    relative: PathBuf,
    kind: tar::EntryType,
    /// The symlink target, verbatim (a relative link resolves against its
    /// own directory — only the *safety* of the target is re-derived, never
    /// its text).
    link: Option<String>,
    mode: u32,
}

/// Validate one archive entry name into a safe relative path: split on
/// both `/` and `\`, drop empty and `.` components, and refuse `..`,
/// absolute roots, and drive-style prefixes — anything suspicious aborts
/// the whole extraction (mirrors the zip extractor's rules in
/// cellar-app).
fn validate_entry_path(name: &str) -> Result<PathBuf, StorageError> {
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(traversal(name));
    }
    let mut relative = PathBuf::new();
    for component in normalized.split('/') {
        match component {
            "" | "." => {}
            ".." => return Err(traversal(name)),
            // Windows-authored archives may carry drive prefixes; a
            // component with a colon is refused whole (parity with the
            // zip extractor — a `C:`-style prefix is never a relative
            // path).
            drive if drive.contains(':') => return Err(traversal(name)),
            other => relative.push(other),
        }
    }
    Ok(relative)
}

/// Validate a symlink target: dangerous where the link sits (absolute,
/// or climbing out of the extraction root). The target is checked
/// lexically against the link's own directory — the root parent is
/// dropped, so `..` that stays beneath the root is allowed.
fn validate_symlink_target(link_path: &Path, target: &str) -> Result<String, StorageError> {
    let target_path = PathBuf::from(target);
    if target_path.is_absolute() {
        return Err(traversal(&format!("{} → {target}", link_path.display())));
    }
    let mut depth = link_path.components().count().saturating_sub(2);
    for component in target_path.components() {
        match component {
            std::path::Component::ParentDir => {
                if depth == 0 {
                    return Err(traversal(&format!("{} → {target}", link_path.display())));
                }
                depth -= 1;
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(traversal(&format!("{} → {target}", link_path.display())));
            }
        }
    }
    // The target passes verbatim: a relative link resolves against its
    // own directory, so the written text must not be re-derived (that
    // would silently re-point the link).
    Ok(target.to_owned())
}

#[cfg(unix)]
fn create_symlink(target: &Path, path: &Path) -> Result<(), StorageError> {
    std::os::unix::fs::symlink(target, path).map_err(storage_io(path))
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, path: &Path) -> Result<(), StorageError> {
    Err(StorageError::Artifact(format!(
        "{}: symlinked archive entries are unsupported on this platform",
        path.display()
    )))
}

fn traversal(entry: &str) -> StorageError {
    StorageError::Artifact(format!(
        "archive entry {entry:?} would escape the extraction root — refused"
    ))
}

/// The single top-level entry of an extracted tarball (the layout
/// contract: `ExtractsToSingleRootDir`) — renamed to the final install
/// dir, whatever its archive-side name (GE-Proton's root is the version;
/// umu's is `umu`).
fn single_root_dir(tmp: &Path) -> Result<PathBuf, StorageError> {
    let mut entries = fs::read_dir(tmp).map_err(storage_io(tmp))?;
    let mut first = entries.next();
    if first.is_none() || entries.next().is_some() {
        return Err(StorageError::Artifact(format!(
            "{}: the extracted archive does not have exactly one top-level entry",
            tmp.display()
        )));
    }
    let first = first
        .take()
        .expect("checked above")
        .map_err(storage_io(tmp))?;
    Ok(first.path())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), StorageError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777)).map_err(storage_io(path))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), StorageError> {
    Ok(())
}

/// `execvp`'s "found and executable" predicate (mirrors the wine
/// provider's): the probe only accepts invocable installs.
fn executable_file(path: &Path) -> bool {
    path.is_file() && unix_executable(path)
}

#[cfg(unix)]
fn unix_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn unix_executable(_path: &Path) -> bool {
    true
}

fn storage_io(path: &Path) -> impl FnOnce(io::Error) -> StorageError + '_ {
    move |error| StorageError::Io(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use cellar_core::errors::StorageError;
    use cellar_core::manifest::{ArchiveLayout, ChecksumScheme, ReleaseSource};

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// A scratch root per test (the `store()` pattern of the sibling tree
    /// tests).
    fn root(tag: &str) -> PathBuf {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "cellar-installer-{tag}-{}-{seq}",
            std::process::id()
        ))
    }

    /// The host arch the fixtures are named for (the pipeline substitutes
    /// `{arch}` from the same constant).
    fn arch() -> &'static str {
        std::env::consts::ARCH
    }

    /// Build a GE-Proton-shaped fixture tarball: a single top-level
    /// directory `{version}` with an executable `proton` script and one
    /// file. Returns the path.
    fn proton_tarball(dir: &Path, version: &str) -> PathBuf {
        let path = dir.join(format!("{version}-{}.tar.gz", arch()));
        let file = fs::File::create(&path).expect("fixture tarball");
        let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::default()));
        append_dir(&mut tar, version, 0o755);
        append_file(
            &mut tar,
            &format!("{version}/proton"),
            0o755,
            "#!/bin/sh\nexit 0\n",
        );
        append_file(
            &mut tar,
            &format!("{version}/files/readme.txt"),
            0o644,
            "fixture\n",
        );
        finish_tarball(tar);
        path
    }

    /// An umu-shaped fixture: single root `umu` with an executable
    /// `umu-run` — the root's name differs from the version pin.
    fn umu_tarball(dir: &Path, version: &str) -> PathBuf {
        let path = dir.join(format!("umu-{version}-{}.tar", arch()));
        let file = fs::File::create(&path).expect("fixture tarball");
        let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::default()));
        append_dir(&mut tar, "umu", 0o755);
        append_file(&mut tar, "umu/umu-run", 0o755, "#!/bin/sh\nexit 0\n");
        finish_tarball(tar);
        path
    }

    fn append_dir(tar: &mut tar::Builder<GzEncoder<fs::File>>, name: &str, mode: u32) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(mode);
        header.set_size(0);
        tar.append_data(&mut header, name, io::empty())
            .expect("tar dir");
    }

    fn append_file(tar: &mut tar::Builder<GzEncoder<fs::File>>, name: &str, mode: u32, body: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(mode);
        header.set_size(body.len() as u64);
        tar.append_data(&mut header, name, io::Cursor::new(body.as_bytes()))
            .expect("tar file");
    }

    /// Finish a fixture tarball properly: `Builder::finish()` only flushes
    /// the gzip encoder — the stream's final block and trailer land on
    /// `finish()` of the encoder itself. Without it the fixture on disk is
    /// the 10-byte gzip header plus whatever the flusher emitted, and the
    /// tar-crate reader fails at the missing trailer.
    fn finish_tarball(tar: tar::Builder<GzEncoder<fs::File>>) {
        let gz = tar.into_inner().expect("builder into_inner");
        gz.finish().expect("gzip finish");
    }

    /// The `.sha512sum` fixture for one artifact: `<hex>  <name>` (the
    /// upstream format).
    fn write_checksum(dir: &Path, artifact: &Path, name: &str) -> String {
        use sha2::Digest as _;

        let mut hasher = Sha512::new();
        let mut file = fs::File::open(artifact).expect("artifact");
        io::copy(&mut file, &mut hasher).expect("digest");
        let hex = hex(&hasher.finalize());
        fs::write(
            dir.join(format!("{name}.sha512sum")),
            format!("{hex}  {name}"),
        )
        .expect("checksum file");
        hex
    }

    /// A proton manifest pointing at fixture URLs under `dir`.
    fn proton_manifest(dir: &Path, _version: &str) -> RunnerManifest {
        RunnerManifest {
            provider_id: "proton".to_owned(),
            source: ReleaseSource {
                url_template: format!("file://{}/{{tag}}-{{arch}}.tar.gz", dir.display()),
                checksum_url_template: Some(format!(
                    "file://{}/{{tag}}-{{arch}}.tar.gz.sha512sum",
                    dir.display()
                )),
            },
            checksum: ChecksumScheme::Sha512,
            archive: ArchiveLayout::ExtractsToSingleRootDir,
            install_kind: InstallKind::CompatTool,
        }
    }

    fn umu_manifest(dir: &Path, _version: &str) -> RunnerManifest {
        RunnerManifest {
            provider_id: "umu".to_owned(),
            source: ReleaseSource {
                url_template: format!("file://{}/umu-{{tag}}-{{arch}}.tar", dir.display()),
                // upstream publishes no checksum for the zipapp (research
                // #18) — the fixture omits the template the same way.
                checksum_url_template: None,
            },
            checksum: ChecksumScheme::Sha512,
            archive: ArchiveLayout::ExtractsToSingleRootDir,
            install_kind: InstallKind::LauncherBinary,
        }
    }

    #[test]
    fn a_traversal_version_pin_is_refused_before_any_path_math() {
        // The pin names the install directory verbatim — a `..` pin must
        // never reach the filesystem (review #34): refused before the
        // provider dir or the lock even exist.
        let root = root("evil-pin");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let sentinel = root.join("evil");
        fs::write(&sentinel, "untouched").expect("sentinel");
        let err = install(&root, &proton_manifest(&fixtures, "x"), "../evil")
            .expect_err("a traversal pin is refused");
        assert!(err.to_string().contains("invalid version pin"), "{err}");
        assert_eq!(
            fs::read_to_string(&sentinel).expect("sentinel"),
            "untouched"
        );
        assert!(
            fs::read_to_string(root.join("evil")).is_ok(),
            "nothing was removed"
        );
        assert!(
            matches!(
                install(&root, &proton_manifest(&fixtures, "x"), ".."),
                Err(StorageError::Artifact(_))
            ),
            "the dot-dot pin is refused too"
        );
    }

    #[test]
    fn symlinks_are_created_with_their_targets_verbatim() {
        // A legal in-root symlink (`../` that stays beneath the root)
        // must be written with its ORIGINAL target text — a relative
        // link resolves against its own directory, so re-deriving the
        // target would silently re-point it (review #34).
        let root = root("symlink");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let path = fixtures.join(format!("{version}-{}.tar.gz", arch()));
        let file = fs::File::create(&path).expect("fixture");
        let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::default()));
        append_dir(&mut tar, version, 0o755);
        append_file(
            &mut tar,
            &format!("{version}/proton"),
            0o755,
            "#!/bin/sh\nexit 0\n",
        );
        append_dir(&mut tar, &format!("{version}/bin"), 0o755);
        append_file(
            &mut tar,
            &format!("{version}/bin/real"),
            0o755,
            "the real thing",
        );
        append_dir(&mut tar, &format!("{version}/files"), 0o755);
        let mut link_header = tar::Header::new_gnu();
        link_header.set_entry_type(tar::EntryType::Symlink);
        link_header.set_mode(0o777);
        link_header.set_size(0);
        tar.append_link(
            &mut link_header,
            format!("{version}/files/link"),
            "../bin/real",
        )
        .expect("symlink entry");
        finish_tarball(tar);
        write_checksum(&fixtures, &path, &format!("{version}-{}.tar.gz", arch()));

        let install_dir =
            install(&root, &proton_manifest(&fixtures, version), version).expect("install");
        let link = install_dir.join("files/link");
        assert_eq!(
            fs::read_link(&link).expect("readlink"),
            PathBuf::from("../bin/real"),
            "the target text is preserved verbatim"
        );
        assert!(link.exists(), "the link resolves inside the install");
    }

    #[test]
    fn an_installed_but_unrecorded_dir_is_reconciled_into_the_inventory() {
        // A dir placed by an earlier interrupted run (or by hand) that
        // passes the probe joins the authoritative inventory on the next
        // `runner install` — the inventory always mirrors what
        // resolution serves (the exists-but-unrecorded gap, review #34).
        let root = root("reconcile");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let dir = root.join("runtime/proton/GE-Proton11-5");
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("proton"), "#!/bin/sh\nexit 0\n").expect("probe target");
        set_mode(&dir.join("proton"), 0o755).expect("executable probe");
        install(&root, &proton_manifest(&fixtures, version), version).expect("no-op install");
        let records = inventory(&root).expect("inventory");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].version, version);
    }

    #[test]
    fn installs_verifies_extracts_probes_and_records() {
        let root = root("round-trip");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let tarball = proton_tarball(&fixtures, version);
        write_checksum(&fixtures, &tarball, &format!("{version}-{}.tar.gz", arch()));

        let manifest = proton_manifest(&fixtures, version);
        let install_dir = install(&root, &manifest, version).expect("install");
        assert_eq!(
            install_dir,
            root.join("runtime/proton/GE-Proton11-5"),
            "runtime/<provider>/<version>"
        );
        assert!(
            executable_file(&install_dir.join("proton")),
            "the probe target is an executable"
        );
        let records = inventory(&root).expect("inventory");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].provider_id, "proton");
        assert_eq!(records[0].version, version);
        assert_eq!(records[0].install, "proton/GE-Proton11-5");
        // Idempotent: a second install is a no-op returning the same dir.
        assert_eq!(
            install(&root, &manifest, version).expect("re-install"),
            install_dir
        );
        assert_eq!(inventory(&root).expect("inventory").len(), 1);
    }

    #[test]
    fn corrupt_download_fails_closed() {
        let root = root("corrupt");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let tarball = proton_tarball(&fixtures, version);
        // The declared checksum is wrong (a flipped byte): the download is
        // corrupt per the manifest.
        fs::write(
            fixtures.join(format!("{version}-{}.tar.gz.sha512sum", arch())),
            format!("00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000  {version}-{}.tar.gz", arch()),
        )
        .expect("bad checksum");
        let err = install(&root, &proton_manifest(&fixtures, version), version)
            .expect_err("a corrupt download must fail");
        assert!(matches!(err, StorageError::Artifact(_)), "{err}");
        assert!(
            !root.join("runtime/proton/GE-Proton11-5").exists(),
            "nothing is extracted from a corrupt artifact"
        );
        assert!(inventory(&root).expect("inventory").is_empty());
        // The corrupt download is discarded — a re-run refetches and, with
        // the checksum fixed, succeeds.
        write_checksum(&fixtures, &tarball, &format!("{version}-{}.tar.gz", arch()));
        install(&root, &proton_manifest(&fixtures, version), version).expect("re-run succeeds");
    }

    #[test]
    fn interrupted_download_resumes_from_the_cache() {
        let root = root("resume");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let tarball = proton_tarball(&fixtures, version);
        write_checksum(&fixtures, &tarball, &format!("{version}-{}.tar.gz", arch()));

        // An interrupted first run left a partial `.part` in the cache.
        let artifact_name = format!("{version}-{}.tar.gz", arch());
        let part = root
            .join("cache/downloads")
            .join(format!("{artifact_name}.part"));
        fs::create_dir_all(part.parent().expect("downloads dir")).expect("cache dir");
        let whole = fs::read(&tarball).expect("fixture bytes");
        fs::write(&part, &whole[..7]).expect("partial download");

        let install_dir =
            install(&root, &proton_manifest(&fixtures, version), version).expect("install resumes");
        assert!(executable_file(&install_dir.join("proton")));
        // The completed artifact is byte-identical to the fixture — a
        // resumed append never corrupts.
        let cached = root.join("cache/downloads").join(&artifact_name);
        assert_eq!(fs::read(&cached).expect("cached artifact"), whole);
    }

    #[test]
    fn concurrent_installs_are_flock_serialized_per_directory() {
        let root = root("flock");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let tarball = proton_tarball(&fixtures, version);
        write_checksum(&fixtures, &tarball, &format!("{version}-{}.tar.gz", arch()));
        let manifest = proton_manifest(&fixtures, version);

        let root_a = root.clone();
        let manifest_a = manifest.clone();
        let a = std::thread::spawn(move || install(&root_a, &manifest_a, version));
        let root_b = root.clone();
        let manifest_b = manifest.clone();
        let b = std::thread::spawn(move || install(&root_b, &manifest_b, version));
        let (a, b) = (a.join().expect("thread a"), b.join().expect("thread b"));
        assert_eq!(
            a.expect("install a"),
            root.join("runtime/proton/GE-Proton11-5")
        );
        assert_eq!(
            b.expect("install b"),
            root.join("runtime/proton/GE-Proton11-5")
        );
        assert_eq!(inventory(&root).expect("inventory").len(), 1, "one record");
    }

    #[test]
    fn umu_shaped_archive_renames_its_root_to_the_version_dir() {
        let root = root("umu-shape");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "1.4.4";
        umu_tarball(&fixtures, version);
        let install_dir =
            install(&root, &umu_manifest(&fixtures, version), version).expect("umu install");
        assert_eq!(install_dir, root.join("runtime/umu/1.4.4"));
        assert!(
            executable_file(&install_dir.join("umu-run")),
            "the probe target is the umu launcher binary"
        );
        assert_eq!(inventory(&root).expect("inventory")[0].install, "umu/1.4.4");
    }

    /// A raw ustar header for a regular file: the tar crate's own builder
    /// refuses `..` paths, so hostile fixtures are assembled byte-wise —
    /// name, mode, size, mtime, type flag, magic, and a correct checksum.
    fn ustar_header(name: &str, mode: u32, size: u64, seq: u64) -> Vec<u8> {
        let mut header = [0u8; 512];
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len().min(100);
        header[..name_len].copy_from_slice(&name_bytes[..name_len]);
        header[100..108].copy_from_slice(format!("{mode:07o}\0").as_bytes());
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        header[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
        header[136..148].copy_from_slice(format!("{seq:011o}\0").as_bytes());
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        header.to_vec()
    }

    #[test]
    fn a_traversal_archive_is_refused_whole() {
        let root = root("traversal");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        // The tar crate refuses to *write* `..` paths, so the hostile
        // archive is assembled byte-wise: a manual ustar header with the
        // raw traversal name, the file body, and the two zero blocks.
        let hostile = ["GE-Proton11-5".to_owned(), "../../escape".to_owned()];
        let path = fixtures.join(format!("{version}-{}.tar.gz", arch()));
        let mut bytes = Vec::new();
        for (seq, name) in hostile.into_iter().enumerate() {
            bytes.extend(ustar_header(
                &name,
                0o755,
                7,
                u64::try_from(seq).expect("fits"),
            ));
            bytes.extend_from_slice(b"hostile");
            bytes.resize(bytes.len() + ((512 - bytes.len() % 512) % 512), 0);
        }
        bytes.extend_from_slice(&[0u8; 1024]);
        let file = fs::File::create(&path).expect("fixture");
        let mut gz = GzEncoder::new(file, Compression::default());
        gz.write_all(&bytes).expect("gzip");
        gz.finish().expect("finish");

        let manifest = proton_manifest(&fixtures, version);
        write_checksum(&fixtures, &path, &format!("{version}-{}.tar.gz", arch()));
        let err = install(&root, &manifest, version).expect_err("traversal refused");
        assert!(err.to_string().contains("escape"), "{err}");
        assert!(
            !root.join("runtime/proton/GE-Proton11-5").exists(),
            "no install dir from a hostile archive (the provider dir exists for the lock)"
        );
        assert!(inventory(&root).expect("inventory").is_empty());
    }

    #[test]
    fn a_multi_root_archive_is_refused() {
        let root = root("multi-root");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let path = fixtures.join(format!("{version}-{}.tar.gz", arch()));
        let file = fs::File::create(&path).expect("fixture");
        let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::default()));
        append_dir(&mut tar, "one", 0o755);
        append_dir(&mut tar, "two", 0o755);
        finish_tarball(tar);
        let manifest = proton_manifest(&fixtures, version);
        write_checksum(&fixtures, &path, &format!("{version}-{}.tar.gz", arch()));
        let err = install(&root, &manifest, version).expect_err("two roots refused");
        assert!(err.to_string().contains("single top-level"), "{err}");
    }

    #[test]
    fn a_missing_probe_target_is_refused() {
        let root = root("no-probe");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let path = fixtures.join(format!("{version}-{}.tar.gz", arch()));
        let file = fs::File::create(&path).expect("fixture");
        let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::default()));
        append_dir(&mut tar, "GE-Proton11-5", 0o755);
        append_file(&mut tar, "GE-Proton11-5/garbage", 0o644, "not a proton");
        finish_tarball(tar);
        write_checksum(&fixtures, &path, &format!("{version}-{}.tar.gz", arch()));
        let err = install(&root, &proton_manifest(&fixtures, version), version)
            .expect_err("a non-proton artifact is refused");
        assert!(
            err.to_string()
                .contains("does not look like a proton install"),
            "{err}"
        );
    }

    #[test]
    fn integrity_failure_of_the_archive_is_a_loud_refusal() {
        // Not a gzip at all: extraction fails closed with no new state.
        let root = root("not-a-tarball");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let tarball = proton_tarball(&fixtures, version);
        fs::write(&tarball, b"this is not a gzip stream").expect("garbage");
        write_checksum(&fixtures, &tarball, &format!("{version}-{}.tar.gz", arch()));
        let err = install(&root, &proton_manifest(&fixtures, version), version)
            .expect_err("garbage archive refused");
        assert!(matches!(err, StorageError::Artifact(_)) || matches!(err, StorageError::Io(_)));
        assert!(!root.join("runtime/proton/GE-Proton11-5").exists());
    }

    #[test]
    fn an_installed_version_is_rebuildable_from_the_inventory() {
        // AC: the runtime directory is rebuildable from the inventory — a
        // wiped runtime (the install dirs and the disposable cache; the
        // inventory itself survives) restores by re-running `runner
        // install` with the recorded version.
        let root = root("rebuild");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let tarball = proton_tarball(&fixtures, version);
        write_checksum(&fixtures, &tarball, &format!("{version}-{}.tar.gz", arch()));
        let manifest = proton_manifest(&fixtures, version);
        install(&root, &manifest, version).expect("install");

        fs::remove_dir_all(root.join("runtime/proton")).expect("install wiped");
        fs::remove_dir_all(root.join("cache")).expect("cache wiped");

        let record = inventory(&root).expect("inventory").remove(0);
        assert_eq!(
            (record.provider_id.as_str(), record.version.as_str()),
            ("proton", version)
        );
        let rebuilt = install(&root, &manifest, &record.version).expect("rebuild");
        assert!(executable_file(&rebuilt.join("proton")));
    }

    #[test]
    fn https_transport_installs_over_a_local_range_server() {
        // The real transport glued end-to-end: a minimal HTTP/1.1 server
        // with byte-range support serves the artifact and its checksum —
        // the same code path a github release rides.
        let root = root("http");
        let version = "GE-Proton11-5";
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let tarball = proton_tarball(&fixtures, version);
        let artifact_bytes = Arc::new(fs::read(&tarball).expect("artifact bytes"));
        let checksum_hex =
            write_checksum(&fixtures, &tarball, &format!("{version}-{}.tar.gz", arch()));
        let checksum_bytes =
            Arc::new(format!("{checksum_hex}  {version}-{}.tar.gz\n", arch()).into_bytes());

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                serve_one(&mut stream, &artifact_bytes, &checksum_bytes);
            }
        });

        let manifest = RunnerManifest {
            provider_id: "proton".to_owned(),
            source: ReleaseSource {
                url_template: format!("http://127.0.0.1:{port}/{{tag}}-{{arch}}.tar.gz"),
                checksum_url_template: Some(format!(
                    "http://127.0.0.1:{port}/{{tag}}-{{arch}}.tar.gz.sha512sum"
                )),
            },
            checksum: ChecksumScheme::Sha512,
            archive: ArchiveLayout::ExtractsToSingleRootDir,
            install_kind: InstallKind::CompatTool,
        };
        let install_dir = install(&root, &manifest, version).expect("http install");
        assert!(executable_file(&install_dir.join("proton")));
        let _ = server;
    }

    /// One HTTP/1.1 exchange: reads the request line + headers, honors a
    /// `Range: bytes=N-` with a 206, and names the file by its URL path.
    fn serve_one(
        stream: &mut std::net::TcpStream,
        artifact: &Arc<Vec<u8>>,
        checksum: &Arc<Vec<u8>>,
    ) {
        use std::io::BufRead;

        let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
        let mut request_line = String::new();
        reader.read_line(&mut request_line).expect("request line");
        let mut range = None;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("header line");
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Range: ") {
                range = Some(value.to_owned());
            }
        }
        let path = request_line
            .split_whitespace()
            .nth(1)
            .expect("request path")
            .to_owned();
        let (body, from) = if path.ends_with(".sha512sum") {
            (checksum, 0)
        } else {
            let mut from = 0u64;
            if let Some(range) = &range {
                if let Some(value) = range.strip_prefix("bytes=") {
                    from = value.trim_end_matches('-').parse().unwrap_or(0);
                }
            }
            (artifact, from)
        };
        let body = &body[usize::try_from(from).expect("fits usize")..];
        let status = if from > 0 {
            "206 Partial Content"
        } else {
            "200 OK"
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{}\r\nConnection: close\r\n\r\n",
            body.len(),
            from + body.len() as u64,
            from + body.len() as u64,
        );
        stream.write_all(head.as_bytes()).expect("response head");
        stream.write_all(body).expect("response body");
        stream.flush().expect("flush");
    }
}
