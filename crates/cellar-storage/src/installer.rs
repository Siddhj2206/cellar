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
//!
//! The pipeline never prints (#37): phase progress — download offsets,
//! verify, extract — is *reported* through the caller-supplied callback
//! threaded in from presentation ([`InstallProgress`]), which decides what
//! reaches a screen (the CLI draws stderr lines).

use cellar_core::manifest::{
    InstallKind, LATEST_PIN, ManagedInventory, ManagedRecord, RunnerManifest,
};
use cellar_core::ports::InstallProgress;

use flate2::read::GzDecoder;
use fs2::FileExt;
use sha2::{Digest, Sha512};

use cellar_core::errors::StorageError;

use std::fs;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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

/// The idle-read timeout of the shared agent (#58): bytes-stopped-moving
/// fails the request; slow-but-moving survives. ureq's default connect
/// timeout (30s) stays; there is deliberately no overall request deadline
/// — a 400 MB artifact on slow DSL legitimately exceeds any fixed budget.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Total HTTP attempts for the resumable download path (#58): the first
/// try plus two retries.
const DOWNLOAD_ATTEMPTS: u32 = 3;

/// The one shared HTTP agent (#58), lazily built and reused by every
/// fetch — artifact stream, checksum fetch, latest-tag lookup. Proxy env
/// vars (`http_proxy`/`https_proxy`/`all_proxy`/`no_proxy`) are honored
/// through ureq's `proxy-from-env` feature.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| ureq::AgentBuilder::new().timeout_read(READ_TIMEOUT).build())
}

/// A test-only agent with an injected read timeout: the stall test proves
/// the timeout mechanism at millisecond scale instead of waiting out the
/// production minute (#58).
#[cfg(test)]
fn agent_with_read_timeout(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_read(timeout).build()
}

/// One HTTP attempt's outcome beyond success (#58): fatal failures
/// propagate immediately; transient ones earn another attempt.
#[derive(Debug)]
enum FetchFailure {
    Fatal(StorageError),
    Transient(String),
}

/// The retry backoff for attempt `n` (1-based): 1s/2s/4s plus a sub-250ms
/// jitter so simultaneous installers don't re-stampede the server.
fn backoff_delay(attempt: u32) -> Duration {
    let base_ms = 1000u64 << (attempt - 1);
    let jitter_ms = u64::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |now| now.subsec_nanos() % 250),
    );
    Duration::from_millis(base_ms + jitter_ms)
}

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
    progress: &mut dyn FnMut(InstallProgress),
) -> Result<PathBuf, StorageError> {
    // The pin: a concrete tag passes through; the `latest` sentinel
    // resolves through the provider's release feed (#65) — once, here,
    // so everything downstream (probe, inventory, narration) sees only
    // the concrete tag it resolved to. The resolved tag is validated
    // like any pin below: a hostile feed never picks the directory.
    let version = resolve_pin(manifest, version)?;
    validate_version_pin(&version)?;
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

    let install_dir = provider_dir.join(&version);
    if probe_installed(manifest, &install_dir) {
        // An installed version is a no-op — and it is recorded, so a dir
        // placed by an earlier interrupted run (or by hand) joins the
        // authoritative inventory (reconciliation: the inventory always
        // mirrors what resolution serves).
        record_inventory(root, manifest, &version)?;
        return Ok(install_dir);
    }
    if install_dir.exists() {
        // Exists but broken (missing probe target): replace it — the
        // runtime dir is disposable, re-derivable state (blueprint §6).
        fs::remove_dir_all(&install_dir).map_err(storage_io(&install_dir))?;
    }

    // Fetch + verify: the resumable cache under the tree's disposable
    // downloads dir.
    let artifact_url = substitute(&manifest.source.url_template, &version, arch);
    let artifact_name = artifact_name(&artifact_url)?;
    let downloads = root.join("cache").join("downloads");
    fs::create_dir_all(&downloads).map_err(storage_io(&downloads))?;
    let artifact = downloads.join(&artifact_name);
    let source = ArtifactSource::parse(&artifact_url)
        .ok_or_else(|| StorageError::Artifact(format!("unusable source URL: {artifact_url}")))?;

    // Fetch → verify → extract, with one bounded second chance (#59): a
    // RESUMED download mixing two remote generations fails verification
    // or extraction — that is a stale cache, not corruption. The partial
    // is discarded and the whole segment reruns fresh, exactly once;
    // stderr names it truthfully. A fresh download failing means genuine
    // corruption and keeps today's vocabulary.
    let mut restarted = false;
    loop {
        let resumed = download_resumable(&source, &artifact, progress)?;
        if let Some(template) = &manifest.source.checksum_url_template {
            // The verify phase opens before the digest is even fetched: the
            // tiny `.sha512sum` request and the whole-artifact hash pass are
            // one phase to the user (#37).
            progress(InstallProgress::Verify);
            let checksum_url = substitute(template, &version, arch);
            let checksum = fetch_checksum(&checksum_url, &downloads, &artifact_name, progress)?;
            // The declared checksum is mandatory: a mismatch discards the
            // download and aborts — nothing is extracted, nothing recorded
            // (AC: corrupt downloads fail closed).
            if let Err(error) = verify_sha512(&artifact, &checksum) {
                if resumed && !restarted {
                    restarted = true;
                    eprintln!(
                        "cellar: cached partial no longer matches the remote artifact — \
                         restarting download"
                    );
                    discard_download(&artifact);
                    continue;
                }
                return Err(error);
            }
        }
        // No checksum URL: upstream publishes no digest (umu's zipapp —
        // research #18), so the artifact installs unverified; a truncated
        // download still fails at extraction (fail closed).

        // Extract into a private temp dir, then move the single root into
        // its final name — the install dir appears atomically.
        progress(InstallProgress::Extract);
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = root.join("runtime").join(format!(
            ".install-{}-{}-{seq}",
            manifest.provider_id,
            std::process::id()
        ));
        fs::create_dir_all(&tmp).map_err(storage_io(&tmp))?;
        match extract_tarball(&artifact, &tmp) {
            Ok(()) => {}
            Err(error) => {
                let _ = fs::remove_dir_all(&tmp);
                if resumed && !restarted {
                    // A resumed unverified artifact (umu has no checksum)
                    // that will not unpack is a stale mix (#59): one clean
                    // fresh attempt, and no binary garbage in any error —
                    // this restart happens before the structural error
                    // surfaces.
                    restarted = true;
                    eprintln!(
                        "cellar: cached partial no longer matches the remote artifact — \
                         restarting download"
                    );
                    discard_download(&artifact);
                    continue;
                }
                return Err(error);
            }
        }

        // The extracted root moves into its final name — the install dir
        // appears atomically. Before the rename, one sweep makes every
        // extracted file and directory durable (#62): one-time O(n) next
        // to a multi-gigabyte download.
        let root_entry = single_root_dir(&tmp)?;
        cellar_core::durability::sync_tree(&tmp).map_err(storage_io(&tmp))?;
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
        // The install-dir entry itself is durable before we record (#62).
        cellar_core::durability::sync_parent_dir(&install_dir).map_err(storage_io(&install_dir))?;
        break;
    }
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
    record_inventory(root, manifest, &version)?;

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

/// Resolve the version pin to a concrete tag (#65): any pin passes
/// through unchanged; the [`LATEST_PIN`] sentinel resolves through the
/// provider's release feed (`ReleaseSource::latest_url`). A manifest
/// without a feed loudly refuses `latest` — never a guess — and the
/// resolved tag is validated like any user-typed pin by the caller, so a
/// hostile or broken feed never picks the install directory.
fn resolve_pin(manifest: &RunnerManifest, version: &str) -> Result<String, StorageError> {
    if version != LATEST_PIN {
        return Ok(version.to_owned());
    }
    let api = manifest.source.latest_url.as_deref().ok_or_else(|| {
        StorageError::Artifact(format!(
            "{} publishes no release feed for \"latest\" — pass an explicit version \
             (the release tag, e.g. GE-Proton11-5)",
            manifest.provider_id
        ))
    })?;
    fetch_latest_tag(api)
}

/// Ask the provider's releases-latest page for its newest tag (#65): the
/// URL redirects to the newest release's page, whose path ends in the
/// tag (`…/releases/latest` → `…/releases/tag/GE-Proton11-5`). The
/// website, not the REST API: the unauthenticated API caps at 60
/// requests per hour per IP (shared networks exhaust it instantly,
/// observed live), while the releases page carries no such budget. A
/// response that stayed put resolved nothing and refuses loudly.
fn fetch_latest_tag(url: &str) -> Result<String, StorageError> {
    // The shared agent (#58): the same read timeout and proxy env as
    // every fetch. Single-shot — no `.part` to resume; a loud timeout
    // failure is the contract.
    let response = agent()
        .get(url)
        .set("User-Agent", "cellar")
        .call()
        .map_err(|error| StorageError::Artifact(format!("release feed {url}: {error}")))?;
    if response.status() != 200 {
        return Err(StorageError::Artifact(format!(
            "release feed {url}: unexpected status {}",
            response.status()
        )));
    }
    // Tags that survive pin validation use only unreserved URL
    // characters (alnum, `.`, `_`, `-`) — characters that are never
    // percent-encoded — so the redirected path's last segment *is* the
    // tag text.
    let final_url = response.get_url().to_owned();
    let tag = final_url.rsplit('/').next().unwrap_or_default();
    if tag.is_empty() || tag == "latest" {
        return Err(StorageError::Artifact(format!(
            "release feed {url} resolved to no tag (ended at {final_url})"
        )));
    }
    Ok(tag.to_owned())
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

    /// Stream from byte `from` onward into `out`, reporting `Download`
    /// progress: one event up front (the resume offset — where this
    /// attempt starts), then one per copied chunk. For `https` a `Range`
    /// request is sent when resuming; a server that ignores it (200)
    /// restarts the file from zero — never a corrupted tail append. HTTP
    /// attempts retry transient failures (#58); every path ends with
    /// `sync_all` — bytes reach the disk before the part is promoted.
    ///
    /// `validator` rides `If-Range` on resuming requests and the response's
    /// own freshness validator (`ETag`, else `Last-Modified`) comes back (#59):
    /// `None` from a non-HTTP source.
    fn stream_from(
        &self,
        from: u64,
        out: &mut fs::File,
        progress: &mut dyn FnMut(InstallProgress),
        validator: Option<&str>,
    ) -> Result<Option<String>, StorageError> {
        match self {
            Self::File(path) => {
                let mut input = fs::File::open(path)
                    .map_err(|error| StorageError::Io(format!("{}: {error}", path.display())))?;
                let total = input
                    .metadata()
                    .map_err(|error| StorageError::Io(format!("{}: {error}", path.display())))?
                    .len();
                input.seek(SeekFrom::Start(from)).map_err(|error| {
                    StorageError::Artifact(format!("seek {}: {error}", path.display()))
                })?;
                progress(InstallProgress::Download {
                    offset: from,
                    total: Some(total),
                });
                let mut counted = CountingRead {
                    inner: &mut input,
                    offset: from,
                    total: Some(total),
                    progress,
                };
                io::copy(&mut counted, out)
                    .map_err(|error| StorageError::Io(format!("copy: {error}")))?;
                // A file side-load carries no freshness validator (#59).
                Ok(None)
            }
            Self::Http(url) => {
                // The retry loop (#58): transient failures (connection
                // reset, read timeout, truncated chunked stream, 5xx, 429)
                // earn another attempt, resuming from whatever landed in
                // the `.part` before the failure. Other 4xx are fatal. The
                // retry event rides the #37 callback — the pipeline never
                // prints; presentation decides what a screen sees.
                let mut resume_from = from;
                for attempt in 1..=DOWNLOAD_ATTEMPTS {
                    match ArtifactSource::http_attempt(
                        url,
                        resume_from,
                        out,
                        progress,
                        agent(),
                        validator,
                    ) {
                        Ok(fresh_validator) => {
                            // The part is promoted and hashed next — the
                            // bytes must be on disk first, on every path.
                            out.sync_all()
                                .map_err(|error| StorageError::Io(format!("sync: {error}")))?;
                            return Ok(fresh_validator);
                        }
                        Err(FetchFailure::Fatal(error)) => return Err(error),
                        Err(FetchFailure::Transient(reason)) => {
                            if attempt == DOWNLOAD_ATTEMPTS {
                                return Err(StorageError::Artifact(format!(
                                    "fetch {url}: {reason} — gave up after \
                                     {DOWNLOAD_ATTEMPTS} attempts"
                                )));
                            }
                            let delay = backoff_delay(attempt);
                            progress(InstallProgress::Retrying {
                                attempt: attempt + 1,
                                attempts: DOWNLOAD_ATTEMPTS,
                                delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                                reason: reason.clone(),
                            });
                            std::thread::sleep(delay);
                            resume_from = out
                                .metadata()
                                .map_err(|error| StorageError::Io(format!("part size: {error}")))?
                                .len();
                        }
                    }
                }
                unreachable!("the retry loop returns on its last attempt")
            }
        }
    }

    /// One HTTP attempt of [`ArtifactSource::stream_from`]: request with
    /// the resume `Range` (and `If-Range` when a validator is known,
    /// #59), map statuses and transport errors to fatal-vs-transient
    /// (#58), copy the body into `out`, and report the response's own
    /// freshness validator.
    #[allow(clippy::too_many_arguments)]
    fn http_attempt(
        url: &str,
        from: u64,
        out: &mut fs::File,
        progress: &mut dyn FnMut(InstallProgress),
        agent: &ureq::Agent,
        validator: Option<&str>,
    ) -> Result<Option<String>, FetchFailure> {
        let mut request = agent.get(url);
        if from > 0 {
            request = request.set("Range", &format!("bytes={from}-"));
            // If-Range (#59): resume only when the remote is still the
            // generation the partial bytes came from; a changed artifact
            // answers 200 and we restart from zero instead of appending
            // two generations into one file.
            if let Some(value) = validator {
                request = request.set("If-Range", value);
            }
        }
        let response = match request.call() {
            Ok(response) => response,
            // A stale oversized `.part` (416) restarts from zero below —
            // it needs the response body channel, so it is not an error.
            Err(ureq::Error::Status(status, response)) => {
                if status == 429 || status >= 500 {
                    return Err(FetchFailure::Transient(format!(
                        "unexpected status {status}"
                    )));
                }
                if status == 416 {
                    response
                } else {
                    return Err(FetchFailure::Fatal(StorageError::Artifact(format!(
                        "fetch {url}: unexpected status {status}"
                    ))));
                }
            }
            Err(other) => {
                return Err(FetchFailure::Transient(format!("fetch {url}: {other}")));
            }
        };
        // The server ignored the range (200), or the resume offset
        // exceeds the current content (416): restart from zero, never
        // append a corrupted tail.
        let mut effective_from = from;
        match response.status() {
            200 | 416 => effective_from = 0,
            206 => {}
            status => {
                return Err(FetchFailure::Fatal(StorageError::Artifact(format!(
                    "fetch {url}: unexpected status {status}"
                ))));
            }
        }
        // A range response's Content-Length names only the served
        // remainder; the artifact total is that plus the resume offset.
        // No length header (chunked) → no total to report.
        let total = response
            .header("Content-Length")
            .and_then(|length| length.parse().ok())
            .map(|length: u64| effective_from + length);
        if effective_from == 0 {
            out.set_len(0).map_err(|error| {
                FetchFailure::Fatal(StorageError::Io(format!("truncate: {error}")))
            })?;
            out.seek(SeekFrom::Start(0))
                .map_err(|error| FetchFailure::Fatal(StorageError::Io(format!("seek: {error}"))))?;
        }
        progress(InstallProgress::Download {
            offset: effective_from,
            total,
        });
        // Freshness validator for the sidecar (#59): ETag first, else
        // Last-Modified — exactly what If-Range accepts.
        let fresh_validator = response
            .header("ETag")
            .or_else(|| response.header("Last-Modified"))
            .map(str::to_owned);
        let mut reader = response.into_reader();
        let mut counted = CountingRead {
            inner: &mut reader,
            offset: effective_from,
            total,
            progress,
        };
        io::copy(&mut counted, out).map_err(|error| match error.kind() {
            // A body that stops arriving — reset, EOF, timeout, or a
            // truncated chunked stream — is the network misbehaving, not
            // the disk (#58). ureq wraps its protocol errors in opaque
            // kinds, so the message joins the classification.
            io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::InvalidData => {
                FetchFailure::Transient(format!("connection lost mid-body: {error}"))
            }
            _ => {
                // ureq wraps its protocol errors (truncated chunked
                // streams included) in opaque kinds, so the message joins
                // the classification (#58, #59).
                let text = error.to_string().to_lowercase();
                let transient_by_text = ["chunk", "timed out", "connection", "reset", "eof"]
                    .iter()
                    .any(|needle| text.contains(needle));
                if transient_by_text {
                    FetchFailure::Transient(format!("connection lost mid-body: {error}"))
                } else {
                    FetchFailure::Fatal(StorageError::Io(format!("copy: {error}")))
                }
            }
        })?;
        Ok(fresh_validator)
    }
}

/// A Read adapter that counts forwarded bytes and reports each chunk as a
/// `Download` progress event (#37) — the byte offset rides the copy loop
/// itself, so the percentage is nearly free. One final zero-byte read at
/// EOF also reports, pinning the last tick at exactly `total`.
struct CountingRead<'a, R: io::Read> {
    inner: R,
    offset: u64,
    total: Option<u64>,
    progress: &'a mut dyn FnMut(InstallProgress),
}

impl<R: io::Read> io::Read for CountingRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.offset += u64::try_from(read).unwrap_or(u64::MAX);
        (self.progress)(InstallProgress::Download {
            offset: self.offset,
            total: self.total,
        });
        Ok(read)
    }
}

/// The freshness sidecar of a `.part` (`foo.tar.part.meta`): the
/// validator — `ETag`, else `Last-Modified` — of the response the partial
/// bytes came from (#59). Missing or empty means "no validator":
/// pre-fix leftovers included, and no resume happens without one.
fn part_meta_path(part: &Path) -> PathBuf {
    let mut os = part.as_os_str().to_os_string();
    os.push(".meta");
    PathBuf::from(os)
}

fn read_validator(meta: &Path) -> Option<String> {
    fs::read_to_string(meta)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Drop the partial download and its freshness sidecar — the paired
/// discard for a stale or corrupt transfer (#59).
fn discard_part_and_meta(part: &Path) {
    let _ = fs::remove_file(part);
    let _ = fs::remove_file(part_meta_path(part));
}

/// Drop a completed-or-partial artifact download with its part and
/// sidecar — the prelude of a #59 fresh restart.
fn discard_download(artifact: &Path) {
    let _ = fs::remove_file(artifact);
    discard_part_and_meta(&artifact.with_extension("part"));
}

/// The resumable download: continue from the `.part` file's current size
/// when its sidecar validator is known (the request rides `If-Range`),
/// or start fresh — no validator, no resume (#59). The part becomes the
/// artifact file only after verification (the caller renames it post-
/// checksum — a corrupt download is deleted, never cached as complete).
///
/// Returns whether this call RESUMED a partial (`from > 0`): a resumed
/// download that later fails verification earns one automatic fresh
/// restart, not a "corrupt download" verdict (#59).
///
/// A terminal fetch failure keeps a part the next run can resume — bytes
/// *and* the validator sidecar #59 needs. A part missing either carries
/// nothing reusable, so it goes: the disposable cache never accumulates
/// debris from attempts that never landed resumable bytes (#46).
fn download_resumable(
    source: &ArtifactSource,
    artifact: &Path,
    progress: &mut dyn FnMut(InstallProgress),
) -> Result<bool, StorageError> {
    if artifact.is_file() {
        // A previously completed and verified artifact in the disposable
        // cache — reuse it (the cache is disposable: remove it to fetch
        // again). No download happens, so no Download events fire; verify
        // and extract still report.
        // Cached-complete reuse: not a resume — no restart eligibility.
        return Ok(false);
    }
    let part = artifact.with_extension("part");
    let meta = part_meta_path(&part);
    let validator = read_validator(&meta);
    let mut out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&part)
        .map_err(storage_io(&part))?;
    let mut from = out.metadata().map_err(storage_io(&part))?.len();
    // No validator → no resume (#59): Content-Length is not a validator,
    // and a pre-fix leftover `.part` has no sidecar — both start fresh,
    // which also creates the sidecar this download will use.
    if from > 0 && validator.is_none() {
        out.set_len(0).map_err(storage_io(&part))?;
        out.seek(SeekFrom::Start(0)).map_err(storage_io(&part))?;
        from = 0;
    }
    let resumed = from > 0;
    let fresh_validator = match source.stream_from(from, &mut out, progress, validator.as_deref()) {
        Ok(fresh_validator) => fresh_validator,
        Err(error) => {
            // A `.part` is resume material only when the next run can
            // actually resume it, and #59 resumes on bytes *plus* a
            // validator sidecar — a sidecar-less part is truncated at the
            // next attempt regardless of its length. So the keep/discard
            // test is "resumable", not "non-empty" (#46): bytes a failed
            // attempt wrote without a sidecar are debris, exactly like an
            // empty part, because nothing will ever read them.
            //
            // A size that cannot be read at all keeps the part: unmeasured
            // is not the same as empty, and deletion is the irreversible
            // branch.
            let resumable = match out.metadata() {
                Ok(status) => status.len() > 0 && validator.is_some(),
                Err(_) => true,
            };
            if !resumable {
                // Closed before the unlink so the removal works on
                // platforms that refuse to delete an open file.
                drop(out);
                discard_part_and_meta(&part);
            }
            return Err(StorageError::Artifact(format!(
                "download to {}: {error}",
                part.display()
            )));
        }
    };
    match fresh_validator {
        Some(value) => {
            let _ = fs::write(&meta, value);
        }
        // A source without validators cannot keep a sidecar honest.
        None => {
            let _ = fs::remove_file(&meta);
        }
    }
    // The download completed: promote the part for verification and drop
    // the now-meaningless sidecar (#59 lifecycle).
    fs::rename(&part, artifact).map_err(storage_io(artifact))?;
    let _ = fs::remove_file(&meta);
    Ok(resumed)
}

/// Fetch the published checksum file (`.sha512sum`), find the line naming
/// our artifact, and return its hex digest. A checksum the manifest
/// declares but cannot be fetched is a hard failure — verification is
/// mandatory when the manifest names a checksum source (AC: corrupt
/// downloads fail closed). Retry diagnostics ride the progress callback;
/// byte ticks do not (#37, #58).
fn fetch_checksum(
    url: &str,
    downloads: &Path,
    artifact_name: &str,
    progress: &mut dyn FnMut(InstallProgress),
) -> Result<String, StorageError> {
    let source = ArtifactSource::parse(url)
        .ok_or_else(|| StorageError::Artifact(format!("unusable checksum URL: {url}")))?;
    let file = downloads.join(format!("{artifact_name}.sha512sum"));
    let mut out = fs::File::create(&file).map_err(storage_io(&file))?;
    let mut filtered = |event: InstallProgress| match event {
        InstallProgress::Download { .. } => {}
        other => progress(other),
    };
    // A failed fetch leaves the checksum file truncated or empty, and the
    // next run re-creates it from scratch anyway — the same debris the
    // `.part` fix removes, in the same directory nothing sweeps (#46).
    if let Err(error) = source.stream_from(0, &mut out, &mut filtered, None) {
        // Closed before the unlink so the removal also works where deleting
        // an open file is refused.
        drop(out);
        let _ = fs::remove_file(&file);
        return Err(error);
    }
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
// Extraction: tar (gzip-detected by content, never by file name) into a
// private temp dir, then a single-root move. Mirrors the zip extractor's
// two-pass vetting (cellar-app archive.rs): the whole archive is read and
// vetted before anything is written.
// ---------------------------------------------------------------------------

/// The gzip magic number: the first two bytes of any gzip stream.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// The archive reader for one artifact: gzip-decompressed when the bytes
/// carry the gzip magic, raw otherwise. Upstream publishes both shapes —
/// GE-Proton's `.tar.gz` and umu's plain `.tar` zipapp — and the content
/// decides, never the file name (#65 follow-up: the zipapp failed under a
/// blanket `GzDecoder` with "invalid gzip header").
fn archive_reader(file: fs::File) -> Result<Box<dyn io::Read>, StorageError> {
    use std::io::Read;

    let mut file = file;
    let mut magic = [0u8; 2];
    let mut filled = 0;
    while filled < magic.len() {
        match file.read(&mut magic[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(error) => return Err(StorageError::Io(format!("archive probe: {error}"))),
        }
    }
    let is_gzip = filled == magic.len() && magic == GZIP_MAGIC;
    // Both readers must see the whole stream: rewind past the probed
    // magic either way.
    file.rewind()
        .map_err(|error| StorageError::Io(format!("archive rewind: {error}")))?;
    if is_gzip {
        Ok(Box::new(GzDecoder::new(file)))
    } else {
        Ok(Box::new(file))
    }
}

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
    let mut tar = tar::Archive::new(archive_reader(file)?);
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
    let mut tar = tar::Archive::new(archive_reader(file)?);
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

    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

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

    /// Install with progress discarded — most of these tests are not
    /// about #37's callback.
    fn install_quiet(
        root: &Path,
        manifest: &RunnerManifest,
        version: &str,
    ) -> Result<PathBuf, StorageError> {
        install(root, manifest, version, &mut |_| {})
    }

    /// A shared event sink for the #37 tests: hand the returned closure
    /// to `install`, read `events` afterwards. The callback is `FnMut`,
    /// so shared ownership keeps the sequence assertable post-return.
    fn collector() -> (
        Rc<RefCell<Vec<InstallProgress>>>,
        impl FnMut(InstallProgress),
    ) {
        let events = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&events);
        (events, move |event| sink.borrow_mut().push(event))
    }

    /// The event sequence as phase tags — `D`ownload ticks, `V`erify,
    /// `E`xtract — so order assertions read like the pipeline's shape.
    fn tags(events: &[InstallProgress]) -> Vec<char> {
        events
            .iter()
            .map(|event| match event {
                InstallProgress::Download { .. } => 'D',
                InstallProgress::Verify => 'V',
                InstallProgress::Extract => 'E',
                InstallProgress::Retrying { .. } => 'R',
            })
            .collect()
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

    /// An umu-shaped fixture — and a truthful one: upstream's zipapp is
    /// a *plain* tar (the #65 follow-up: a blanket `GzDecoder` failed it
    /// with "invalid gzip header"), so this fixture is uncompressed too,
    /// exercising the content-sniffed raw extraction path. Single root
    /// `umu` with an executable `umu-run` — the root's name differs from
    /// the version pin.
    fn umu_tarball(dir: &Path, version: &str) -> PathBuf {
        let path = dir.join(format!("umu-{version}-{}.tar", arch()));
        let file = fs::File::create(&path).expect("fixture tarball");
        let mut tar = tar::Builder::new(file);
        append_dir(&mut tar, "umu", 0o755);
        append_file(&mut tar, "umu/umu-run", 0o755, "#!/bin/sh\nexit 0\n");
        tar.finish().expect("tar trailer");
        path
    }

    fn append_dir<W: std::io::Write>(tar: &mut tar::Builder<W>, name: &str, mode: u32) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(mode);
        header.set_size(0);
        tar.append_data(&mut header, name, io::empty())
            .expect("tar dir");
    }

    fn append_file<W: std::io::Write>(
        tar: &mut tar::Builder<W>,
        name: &str,
        mode: u32,
        body: &str,
    ) {
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
                latest_url: None,
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
                latest_url: None,
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
        let err = install_quiet(&root, &proton_manifest(&fixtures, "x"), "../evil")
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
                install_quiet(&root, &proton_manifest(&fixtures, "x"), ".."),
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
            install_quiet(&root, &proton_manifest(&fixtures, version), version).expect("install");
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
        install_quiet(&root, &proton_manifest(&fixtures, version), version).expect("no-op install");
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
        let install_dir = install_quiet(&root, &manifest, version).expect("install");
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
            install_quiet(&root, &manifest, version).expect("re-install"),
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
        let err = install_quiet(&root, &proton_manifest(&fixtures, version), version)
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
        install_quiet(&root, &proton_manifest(&fixtures, version), version)
            .expect("re-run succeeds");
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
        // The freshness sidecar (#59): without it the pipeline must not
        // resume — the seed includes one so the offset-7 contract holds.
        let meta = PathBuf::from(format!("{}.meta", part.display()));
        fs::write(&meta, "\"etag-resume\"").expect("sidecar");

        let install_dir = install_quiet(&root, &proton_manifest(&fixtures, version), version)
            .expect("install resumes");
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
        let a = std::thread::spawn(move || install_quiet(&root_a, &manifest_a, version));
        let root_b = root.clone();
        let manifest_b = manifest.clone();
        let b = std::thread::spawn(move || install_quiet(&root_b, &manifest_b, version));
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
            install_quiet(&root, &umu_manifest(&fixtures, version), version).expect("umu install");
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
        let err = install_quiet(&root, &manifest, version).expect_err("traversal refused");
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
        let err = install_quiet(&root, &manifest, version).expect_err("two roots refused");
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
        let err = install_quiet(&root, &proton_manifest(&fixtures, version), version)
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
        let err = install_quiet(&root, &proton_manifest(&fixtures, version), version)
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
        install_quiet(&root, &manifest, version).expect("install");

        fs::remove_dir_all(root.join("runtime/proton")).expect("install wiped");
        fs::remove_dir_all(root.join("cache")).expect("cache wiped");

        let record = inventory(&root).expect("inventory").remove(0);
        assert_eq!(
            (record.provider_id.as_str(), record.version.as_str()),
            ("proton", version)
        );
        let rebuilt = install_quiet(&root, &manifest, &record.version).expect("rebuild");
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
        let (port, server) =
            serve_locally(artifact_bytes, checksum_bytes, feed_tag("GE-Proton11-5"));

        let manifest = RunnerManifest {
            provider_id: "proton".to_owned(),
            source: ReleaseSource {
                url_template: format!("http://127.0.0.1:{port}/{{tag}}-{{arch}}.tar.gz"),
                checksum_url_template: Some(format!(
                    "http://127.0.0.1:{port}/{{tag}}-{{arch}}.tar.gz.sha512sum"
                )),
                latest_url: None,
            },
            checksum: ChecksumScheme::Sha512,
            archive: ArchiveLayout::ExtractsToSingleRootDir,
            install_kind: InstallKind::CompatTool,
        };
        let install_dir = install_quiet(&root, &manifest, version).expect("http install");
        assert!(executable_file(&install_dir.join("proton")));
        let _ = server;
    }

    #[test]
    fn progress_reports_download_ticks_then_verify_then_extract() {
        // AC (#37): the callback event sequence over the real transport —
        // download ticks first (offsets rising to the artifact size), one
        // Verify, then Extract last. The local Range-capable server from
        // #34 serves the bytes.
        let root = root("progress-http");
        let version = "GE-Proton11-5";
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let tarball = proton_tarball(&fixtures, version);
        let artifact_bytes = Arc::new(fs::read(&tarball).expect("artifact bytes"));
        let checksum_hex =
            write_checksum(&fixtures, &tarball, &format!("{version}-{}.tar.gz", arch()));
        let checksum_bytes =
            Arc::new(format!("{checksum_hex}  {version}-{}.tar.gz\n", arch()).into_bytes());
        let (port, server) =
            serve_locally(artifact_bytes, checksum_bytes, feed_tag("GE-Proton11-5"));

        let manifest = RunnerManifest {
            provider_id: "proton".to_owned(),
            source: ReleaseSource {
                url_template: format!("http://127.0.0.1:{port}/{{tag}}-{{arch}}.tar.gz"),
                checksum_url_template: Some(format!(
                    "http://127.0.0.1:{port}/{{tag}}-{{arch}}.tar.gz.sha512sum"
                )),
                latest_url: None,
            },
            checksum: ChecksumScheme::Sha512,
            archive: ArchiveLayout::ExtractsToSingleRootDir,
            install_kind: InstallKind::CompatTool,
        };
        let (events, mut report) = collector();
        install(&root, &manifest, version, &mut report).expect("http install with progress");

        let events = events.borrow();
        let tags = tags(&events);
        assert_eq!(
            tags.last(),
            Some(&'E'),
            "extraction is the last phase: {events:?}"
        );
        assert_eq!(
            tags.iter().rev().take(2).collect::<Vec<_>>(),
            vec![&'E', &'V'],
            "verify directly precedes extract: {events:?}"
        );
        assert!(
            tags[..tags.len() - 2].iter().all(|tag| *tag == 'D'),
            "every earlier event is a download tick (no checksum-fetch noise): {events:?}"
        );

        let total = u64::try_from(
            fs::read(
                root.join("cache")
                    .join("downloads")
                    .join(format!("{version}-{}.tar.gz", arch())),
            )
            .expect("cached artifact")
            .len(),
        )
        .expect("fits");
        let mut previous = 0u64;
        for event in events.iter() {
            let InstallProgress::Download {
                offset,
                total: announced,
            } = event
            else {
                break;
            };
            assert_eq!(*announced, Some(total), "the total is known and stable");
            assert!(*offset >= previous, "offsets never go back: {event:?}");
            previous = *offset;
        }
        assert_eq!(previous, total, "the last tick lands exactly on the total");
        let _ = server;
    }

    #[test]
    fn a_resumed_download_reports_the_resume_offset_first() {
        // The initial tick names where this attempt starts — a resumed
        // `.part` continues at its own length, so the CLI's percentage
        // picks up mid-download instead of restarting at zero.
        let root = root("progress-resume");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let tarball = proton_tarball(&fixtures, version);
        write_checksum(&fixtures, &tarball, &format!("{version}-{}.tar.gz", arch()));
        let whole = fs::read(&tarball).expect("fixture bytes");
        // The pipeline's part name: `with_extension("part")` on the
        // artifact (`…x86_64.tar.gz`) replaces only its last extension —
        // the fixture must sit exactly where the resume looks.
        let part = root
            .join("cache/downloads")
            .join(format!("{version}-{}.tar.part", arch()));
        fs::create_dir_all(part.parent().expect("downloads dir")).expect("cache dir");
        fs::write(&part, &whole[..7]).expect("partial download");
        // The freshness sidecar (#59): no validator, no resume — seeding
        // it keeps the offset-7 contract meaningful.
        let meta = PathBuf::from(format!("{}.meta", part.display()));
        fs::write(&meta, "etag-resume").expect("sidecar");

        let manifest = proton_manifest(&fixtures, version);
        let (events, mut report) = collector();
        install(&root, &manifest, version, &mut report).expect("resumed install");

        let events = events.borrow();
        let total = u64::try_from(whole.len()).expect("fits");
        match events.first() {
            Some(InstallProgress::Download {
                offset,
                total: announced,
            }) => {
                assert_eq!(*offset, 7, "the resume offset opens the phase");
                assert_eq!(*announced, Some(total));
            }
            other => panic!("the first event is a download tick, got {other:?}"),
        }
        let last_download = events
            .iter()
            .rev()
            .find(|event| matches!(event, InstallProgress::Download { .. }))
            .expect("at least one download tick");
        assert_eq!(
            last_download,
            &InstallProgress::Download {
                offset: total,
                total: Some(total)
            },
            "the final tick lands on the completed artifact"
        );
        // And the phases still close in order around the ticks.
        assert!(tags(&events).ends_with(&['V', 'E']), "{events:?}");
    }

    #[test]
    fn a_cached_artifact_skips_download_but_still_verifies_and_extracts() {
        // A complete artifact in the disposable cache means no download
        // runs — so no Download events fire — but verify and extract are
        // real phases of the run and report as such (#37).
        let root = root("progress-cached");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let tarball = proton_tarball(&fixtures, version);
        write_checksum(&fixtures, &tarball, &format!("{version}-{}.tar.gz", arch()));
        let manifest = proton_manifest(&fixtures, version);
        install_quiet(&root, &manifest, version).expect("first install fills the cache");

        // Wipe the runtime: the next install reuses only the cached
        // artifact (the idempotent no-op would fire no phases at all).
        fs::remove_dir_all(root.join("runtime/proton")).expect("runtime wiped");
        let (events, mut report) = collector();
        install(&root, &manifest, version, &mut report).expect("install from cache");

        assert_eq!(
            events.borrow().as_slice(),
            [InstallProgress::Verify, InstallProgress::Extract],
            "no download ticks for a cached artifact"
        );
    }

    #[test]
    fn a_checksum_less_manifest_skips_the_verify_phase() {
        // umu publishes no digest (research #18): its manifest names no
        // checksum source, so there is nothing to verify — the event
        // sequence goes straight from download ticks to extract.
        let root = root("progress-umu");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "1.4.4";
        umu_tarball(&fixtures, version);
        let manifest = umu_manifest(&fixtures, version);
        let (events, mut report) = collector();
        install(&root, &manifest, version, &mut report).expect("umu install with progress");

        let tags = tags(&events.borrow());
        assert!(
            !tags.contains(&'V') && tags.ends_with(&['D', 'E']),
            "download ticks then extract, never verify: {tags:?}"
        );
    }

    /// One HTTP/1.1 exchange: reads the request line + headers, honors a
    /// `Range: bytes=N-` with a 206, and names the file by its URL path.
    /// A `/releases/latest` path serves the feed body instead (the
    /// GitHub-releases-API shape `latest` resolution rides, #65).
    fn serve_one(
        stream: &mut std::net::TcpStream,
        artifact: &Arc<Vec<u8>>,
        checksum: &Arc<Vec<u8>>,
        feed: &Arc<Vec<u8>>,
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
        // The release-feed branch (#65): the tag text in `feed` rides a
        // 302's Location path — exactly how the real releases-latest
        // page hands out its newest tag.
        if path.contains("/releases/latest") {
            let tag = std::str::from_utf8(feed).expect("feed tag");
            let head = format!(
                "HTTP/1.1 302 Found\r\nLocation: /releases/tag/{tag}\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(head.as_bytes()).expect("redirect");
            stream.flush().expect("flush");
            return;
        }
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

    /// The local Range-capable server of the #34 fixture, extended with
    /// the release-feed path (#65): one artifact, its checksum, and a
    /// GitHub-API-shaped feed body. Returns the bound port and the
    /// server's join handle (kept alive for the test's duration).
    fn serve_locally(
        artifact: Arc<Vec<u8>>,
        checksum: Arc<Vec<u8>>,
        feed: Arc<Vec<u8>>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                serve_one(&mut stream, &artifact, &checksum, &feed);
            }
        });
        (port, server)
    }

    /// The release-feed fixture (#65): the tag the `/releases/latest`
    /// branch answers its 302 redirect with — the same way the real
    /// releases-latest page hands out the newest tag.
    fn feed_tag(tag: &str) -> Arc<Vec<u8>> {
        Arc::new(tag.as_bytes().to_vec())
    }

    // -----------------------------------------------------------------
    // The #58 retry/timeout fixtures: a scripted server, hermetic against
    // the real network — one raw response per connection, popped in
    // order; a truncated response is a connection lost mid-body.
    // -----------------------------------------------------------------
    /// A scripted HTTP server: each connection pops the next raw response
    /// and every request's `Range` header is recorded. Dropping the
    /// response short is how a "connection lost mid-body" is staged.
    fn serve_scripted(
        responses: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        seen_requests: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            use std::io::BufRead;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut queue = responses
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if queue.is_empty() {
                    continue;
                }
                let response = queue.remove(0);
                drop(queue);
                let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
                let mut request_line = String::new();
                reader.read_line(&mut request_line).expect("request line");
                let mut range = None;
                let mut if_range = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let line = line.trim_end().to_owned();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("Range: ") {
                        range = Some(value.to_owned());
                    }
                    if let Some(value) = line.strip_prefix("If-Range: ") {
                        if_range = Some(value.to_owned());
                    }
                }
                seen_requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(format!("range={range:?}; if_range={if_range:?}"));
                stream.write_all(&response).expect("scripted response");
                stream.flush().expect("flush");
                // The socket drops here — a truncated body is exactly a
                // connection lost mid-transfer.
            }
        });
        (port, server)
    }

    fn body_response(body: &[u8]) -> Vec<u8> {
        let mut raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    fn range_response(body: &[u8]) -> Vec<u8> {
        let mut raw = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    fn status_response(status: u16) -> Vec<u8> {
        format!("HTTP/1.1 {status} nope\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .into_bytes()
    }

    #[test]
    fn a_connection_cut_mid_body_retries_and_resumes_from_the_part() {
        // AC (#58): attempt 1 delivers 10 of 64 promised bytes then the
        // socket drops; the retry resumes from the `.part` — the second
        // request carries `Range: bytes=10-` and a 206 completes the
        // artifact exactly.
        let total: Vec<u8> = (0..64u8).collect();
        // Attempt 1: chunked body cut before the terminating zero chunk —
        // the decoder errors mid-stream (a connection lost mid-body).
        // Attempt 2: the resume request served as a complete range.
        let mut truncated =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        truncated.extend_from_slice(b"a\r\n");
        truncated.extend_from_slice(&total[..10]);
        truncated.extend_from_slice(b"\r\n");
        let responses = Arc::new(std::sync::Mutex::new(vec![
            truncated,
            range_response(&total[10..]),
        ]));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (port, server) = serve_scripted(responses.clone(), requests.clone());
        let root = root("retry-resume");
        fs::create_dir_all(&root).unwrap();
        let artifact = root.join("artifact.tar.gz");
        download_resumable(
            &ArtifactSource::Http(format!("http://127.0.0.1:{port}/artifact")),
            &artifact,
            &mut |_| {},
        )
        .expect("the retry completes the download");
        assert_eq!(fs::read(&artifact).unwrap(), total, "bytes exact");
        let requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(requests.len(), 2, "exactly one retry: {requests:?}");
        assert!(
            requests[0].contains("range=None"),
            "the first try starts from zero: {:?}",
            requests[0]
        );
        assert!(
            requests[1].contains("range=Some(\"bytes=10-\")"),
            "the retry resumes from the part: {:?}",
            requests[1]
        );
        drop(server); // the accept loop runs until process exit — joining would block forever
    }

    #[test]
    fn a_fatal_status_fails_immediately_without_retry() {
        // AC (#58): other 4xx never earn an attempt — a 404 will not fix
        // itself.
        let responses = Arc::new(std::sync::Mutex::new(vec![status_response(404)]));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (port, server) = serve_scripted(responses.clone(), requests.clone());
        let root = root("retry-404");
        fs::create_dir_all(&root).unwrap();
        let error = download_resumable(
            &ArtifactSource::Http(format!("http://127.0.0.1:{port}/artifact")),
            &root.join("artifact.tar.gz"),
            &mut |_| {},
        )
        .expect_err("404 is fatal");
        assert!(error.to_string().contains("404"), "{error}");
        assert_eq!(
            requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "no retry for a fatal status"
        );
        drop(server); // the accept loop runs until process exit — joining would block forever
    }

    #[test]
    fn a_terminal_failure_that_landed_no_bytes_leaves_no_part_behind() {
        // AC (#46): a failed attempt that never wrote a byte leaves nothing
        // in the disposable cache — an empty `.part` can never resume, and
        // the next run truncates it anyway.
        let responses = Arc::new(std::sync::Mutex::new(vec![status_response(404)]));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (port, server) = serve_scripted(responses.clone(), requests.clone());
        let root = root("empty-part");
        fs::create_dir_all(&root).unwrap();
        let artifact = root.join("artifact.tar.gz");
        let partial = artifact.with_extension("part");
        // Seeded empty part *and* sidecar: an earlier attempt wrote the
        // sidecar but no body byte. Asserting the sidecar is gone is only
        // meaningful when one existed.
        fs::write(&partial, b"").expect("seed empty part");
        fs::write(part_meta_path(&partial), "\"etag-empty\"").expect("seed sidecar");
        download_resumable(
            &ArtifactSource::Http(format!("http://127.0.0.1:{port}/artifact")),
            &artifact,
            &mut |_| {},
        )
        .expect_err("404 is fatal");
        assert!(!partial.exists(), "no empty part in the cache");
        assert!(!part_meta_path(&partial).exists(), "no sidecar either");
        drop(server); // the accept loop runs until process exit — joining would block forever
    }

    #[test]
    fn a_terminal_failure_keeps_a_part_that_can_still_resume() {
        // AC (#46): the other half — bytes on disk are resume material for
        // the next run (#59), so a fatal failure must not throw them away.
        let responses = Arc::new(std::sync::Mutex::new(vec![status_response(404)]));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (port, server) = serve_scripted(responses.clone(), requests.clone());
        let root = root("kept-part");
        fs::create_dir_all(&root).unwrap();
        let artifact = root.join("artifact.tar.gz");
        let partial = artifact.with_extension("part");
        // An earlier interrupted run left seven bytes plus the freshness
        // sidecar that makes them resumable.
        fs::write(&partial, b"1234567").expect("seed partial");
        fs::write(part_meta_path(&partial), "\"etag-keep\"").expect("seed sidecar");
        download_resumable(
            &ArtifactSource::Http(format!("http://127.0.0.1:{port}/artifact")),
            &artifact,
            &mut |_| {},
        )
        .expect_err("404 is fatal");
        assert_eq!(
            fs::metadata(&partial).expect("the partial survives").len(),
            7,
            "the resumable bytes are kept for the next attempt"
        );
        assert!(
            part_meta_path(&partial).exists(),
            "the sidecar survives too — without it the bytes are not resumable"
        );
        drop(server); // the accept loop runs until process exit — joining would block forever
    }

    #[test]
    fn a_terminal_failure_discards_a_part_no_sidecar_can_resume() {
        // AC (#46), the case a length-only rule gets wrong: bytes on disk
        // are resume material only alongside the validator #59 resumes on.
        // Without a sidecar the next attempt truncates the part anyway, so
        // keeping it leaves debris nothing will ever read.
        let responses = Arc::new(std::sync::Mutex::new(vec![status_response(404)]));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (port, server) = serve_scripted(responses.clone(), requests.clone());
        let root = root("part-without-sidecar");
        fs::create_dir_all(&root).unwrap();
        let artifact = root.join("artifact.tar.gz");
        let partial = artifact.with_extension("part");
        // Seeded bytes with no sidecar — the pre-#59 leftover shape.
        fs::write(&partial, b"1234567").expect("seed partial");
        download_resumable(
            &ArtifactSource::Http(format!("http://127.0.0.1:{port}/artifact")),
            &artifact,
            &mut |_| {},
        )
        .expect_err("404 is fatal");
        assert!(
            !partial.exists(),
            "unresumable bytes go the way empty ones do"
        );
        drop(server); // the accept loop runs until process exit — joining would block forever
    }

    #[test]
    fn retries_stop_after_three_attempts_with_backoff() {
        // AC (#58): transient failures (5xx here) are retried up to three
        // attempts total, backing off 1s then 2s between them — the bound
        // and the backoff are both observable in the wall clock.
        let responses = Arc::new(std::sync::Mutex::new(vec![
            status_response(500);
            DOWNLOAD_ATTEMPTS as usize
        ]));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (port, server) = serve_scripted(responses.clone(), requests.clone());
        let root = root("retry-bound");
        fs::create_dir_all(&root).unwrap();
        let started = Instant::now();
        let error = download_resumable(
            &ArtifactSource::Http(format!("http://127.0.0.1:{port}/artifact")),
            &root.join("artifact.tar.gz"),
            &mut |_| {},
        )
        .expect_err("a persistent 500 gives up");
        assert!(
            error.to_string().contains("gave up after 3 attempts"),
            "{error}"
        );
        assert_eq!(
            requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            DOWNLOAD_ATTEMPTS as usize,
            "three attempts, no more"
        );
        assert!(
            started.elapsed() >= Duration::from_secs(3),
            "the 1s + 2s backoffs actually slept: {:?}",
            started.elapsed()
        );
        drop(server); // the accept loop runs until process exit — joining would block forever
    }

    #[test]
    fn a_stalling_server_times_out_instead_of_hanging_forever() {
        // AC (#58): headers promise a mebibyte that never arrives — the
        // idle-read timeout ends the hang. The production timeout is 60s;
        // this exercises the same attempt code at millisecond scale via
        // the injected-timeout agent, against a server that holds the
        // socket open (no EOF to shortcut the timeout).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            use std::io::BufRead;
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if line.trim_end().is_empty() {
                    break;
                }
                line.clear();
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n")
                .expect("headers");
            stream.flush().expect("flush");
            std::thread::sleep(Duration::from_secs(5));
            // No body ever comes; the socket stays open well past the
            // test's timeout window.
        });
        let url = format!("http://127.0.0.1:{port}/artifact");
        let _source = ArtifactSource::Http(url.clone());
        let stall_root = root("stall");
        fs::create_dir_all(&stall_root).unwrap();
        let out_path = stall_root.join("artifact.part");
        let mut out = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&out_path)
            .unwrap();
        let agent = agent_with_read_timeout(Duration::from_millis(250));
        let started = Instant::now();
        let outcome = ArtifactSource::http_attempt(&url, 0, &mut out, &mut |_| {}, &agent, None);
        let elapsed = started.elapsed();
        match outcome {
            Err(FetchFailure::Transient(reason)) => assert!(
                reason.contains("mid-body") || reason.contains("timed out"),
                "the stall reads as a transient failure: {reason}"
            ),
            other => panic!("expected a transient timeout, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(4),
            "the timeout fired well before the server's hold expired: {elapsed:?}"
        );
        drop(server); // the accept loop runs until process exit — joining would block forever
    }

    #[test]
    fn latest_resolves_through_the_feed_and_records_the_concrete_tag() {
        // AC (#65): `latest` resolves once through the provider's feed,
        // then runs the pinned pipeline — the install dir and inventory
        // record the concrete tag, and a second run is the usual no-op.
        let root = root("latest-http");
        let version = "GE-Proton11-5";
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let tarball = proton_tarball(&fixtures, version);
        let artifact_bytes = Arc::new(fs::read(&tarball).expect("artifact bytes"));
        let checksum_hex =
            write_checksum(&fixtures, &tarball, &format!("{version}-{}.tar.gz", arch()));
        let checksum_bytes =
            Arc::new(format!("{checksum_hex}  {version}-{}.tar.gz\n", arch()).into_bytes());
        let (port, server) = serve_locally(artifact_bytes, checksum_bytes, feed_tag(version));

        let manifest = RunnerManifest {
            provider_id: "proton".to_owned(),
            source: ReleaseSource {
                url_template: format!("http://127.0.0.1:{port}/{{tag}}-{{arch}}.tar.gz"),
                checksum_url_template: Some(format!(
                    "http://127.0.0.1:{port}/{{tag}}-{{arch}}.tar.gz.sha512sum"
                )),
                latest_url: Some(format!("http://127.0.0.1:{port}/releases/latest")),
            },
            checksum: ChecksumScheme::Sha512,
            archive: ArchiveLayout::ExtractsToSingleRootDir,
            install_kind: InstallKind::CompatTool,
        };
        let install_dir = install(&root, &manifest, LATEST_PIN, &mut |_| {})
            .expect("latest resolves and installs");
        assert_eq!(
            install_dir,
            root.join("runtime/proton").join(version),
            "the concrete tag names the install directory"
        );
        let records = inventory(&root).expect("inventory");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].version, version, "the concrete tag is recorded");
        // Idempotent: the same feed state re-resolves to the same pin and
        // hits the installed-version probe — a no-op.
        assert_eq!(
            install(&root, &manifest, LATEST_PIN, &mut |_| {}).expect("re-run"),
            install_dir
        );
        assert_eq!(inventory(&root).expect("inventory").len(), 1);
        let _ = server;
    }

    #[test]
    fn latest_without_a_release_feed_is_a_loud_refusal() {
        // A manifest with no `latest_url` offers no latest resolution:
        // refused loudly before any path math or fetch — never guessed
        // from the URL template (AC #65).
        let root = root("latest-no-feed");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let tarball = proton_tarball(&fixtures, version);
        write_checksum(&fixtures, &tarball, &format!("{version}-{}.tar.gz", arch()));
        let mut manifest = proton_manifest(&fixtures, version);
        manifest.source.latest_url = None;

        let err =
            install(&root, &manifest, LATEST_PIN, &mut |_| {}).expect_err("no feed, no latest");
        assert!(err.to_string().contains("no release feed"), "{err}");
        assert!(inventory(&root).expect("inventory").is_empty());
    }

    #[test]
    fn the_archive_format_is_decided_by_content_not_name() {
        // The #65 follow-up bug, pinned: a `.tar.gz`-named URL carrying a
        // *plain* tar (umu's zipapp shape) must extract — the gzip magic
        // in the bytes decides, never the file name. The inverse (gzip
        // bytes through the decoder path) is every proton fixture.
        let root = root("content-not-name");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let gz_name = format!("{version}-{}.tar.gz", arch());
        let plain = fixtures.join(&gz_name);
        let file = fs::File::create(&plain).expect("fixture tarball");
        let mut tar = tar::Builder::new(file);
        append_dir(&mut tar, version, 0o755);
        append_file(
            &mut tar,
            &format!("{version}/proton"),
            0o755,
            "#!/bin/sh\nexit 0\n",
        );
        tar.finish().expect("tar trailer");
        write_checksum(&fixtures, &plain, &gz_name);

        let install_dir =
            install_quiet(&root, &proton_manifest(&fixtures, version), version).expect("install");
        assert!(executable_file(&install_dir.join("proton")));
    }

    #[test]
    fn a_hostile_feed_tag_is_validated_like_a_pin() {
        // The resolved tag picks the install directory, so it must pass
        // the same validation as a typed pin: a traversal tag from a
        // hostile or broken feed is refused whole, nothing installed
        // (AC #65).
        let root = root("latest-evil-feed");
        let fixtures = root.join("fixtures");
        fs::create_dir_all(&fixtures).expect("fixtures dir");
        let version = "GE-Proton11-5";
        let tarball = proton_tarball(&fixtures, version);
        let artifact_bytes = Arc::new(fs::read(&tarball).expect("artifact bytes"));
        let checksum_hex =
            write_checksum(&fixtures, &tarball, &format!("{version}-{}.tar.gz", arch()));
        let checksum_bytes =
            Arc::new(format!("{checksum_hex}  {version}-{}.tar.gz\n", arch()).into_bytes());
        // A percent-encoded traversal tag: it survives the redirect path
        // verbatim (no `/` segmentation to eat it), and pin validation
        // must refuse it whole — nothing installed, nothing recorded.
        let (port, server) =
            serve_locally(artifact_bytes, checksum_bytes, feed_tag("%2E%2E%2Fescape"));

        let manifest = RunnerManifest {
            provider_id: "proton".to_owned(),
            source: ReleaseSource {
                url_template: format!("http://127.0.0.1:{port}/{{tag}}-{{arch}}.tar.gz"),
                checksum_url_template: Some(format!(
                    "http://127.0.0.1:{port}/{{tag}}-{{arch}}.tar.gz.sha512sum"
                )),
                latest_url: Some(format!("http://127.0.0.1:{port}/releases/latest")),
            },
            checksum: ChecksumScheme::Sha512,
            archive: ArchiveLayout::ExtractsToSingleRootDir,
            install_kind: InstallKind::CompatTool,
        };
        let err = install(&root, &manifest, LATEST_PIN, &mut |_| {})
            .expect_err("a traversal tag from the feed is refused");
        assert!(err.to_string().contains("invalid version pin"), "{err}");
        assert!(
            !root.join("runtime/proton").join("%2E%2E%2Fescape").exists(),
            "nothing was installed from the hostile tag"
        );
        assert!(inventory(&root).expect("inventory").is_empty());
        let _ = server;
    }
    #[test]
    fn a_generation_swap_restarts_from_zero_and_rewrites_the_sidecar() {
        // AC (#59): the remote moved on (If-Range mismatch → 200). The
        // partial is abandoned, the fresh body lands whole, and the
        // sidecar carries the new generation.
        let v1: Vec<u8> = vec![0xAA; 10];
        let v2: Vec<u8> = vec![0xBB; 64];
        let mut swap =
            b"HTTP/1.1 200 OK\r\nETag: \"v2\"\r\nContent-Length: 64\r\nConnection: close\r\n\r\n"
                .to_vec();
        swap.extend_from_slice(&v2);
        let responses = Arc::new(std::sync::Mutex::new(vec![swap]));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (port, server) = serve_scripted(responses.clone(), requests.clone());
        let root = root("swap");
        fs::create_dir_all(&root).unwrap();
        let artifact = root.join("artifact.bin");
        // Seed: v1 partial + its sidecar validator.
        let stale_part = artifact.with_extension("part");
        fs::write(&stale_part, &v1).unwrap();
        let meta = part_meta_path(&stale_part);
        fs::write(&meta, "v1").unwrap();

        download_resumable(
            &ArtifactSource::Http(format!("http://127.0.0.1:{port}/artifact")),
            &artifact,
            &mut |_| {},
        )
        .expect("the swap download completes");

        assert_eq!(fs::read(&artifact).unwrap(), v2, "pure v2, no mix");
        // The resume attempt rode If-Range with the stale generation.
        let first = &requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[0];
        assert!(
            first.contains("if_range=Some(\"v1\")") && first.contains("range=Some(\"bytes=10-\")"),
            "the validator rode the wire: {first}"
        );
        // Lifecycle (#59): the promoted artifact leaves no sidecar behind.
        assert!(!meta.exists(), "the sidecar is dropped on promotion");
        drop(server);
    }

    #[test]
    fn no_sidecar_means_a_fresh_download_that_creates_one() {
        // AC (#59): a pre-fix `.part` without a sidecar is not trusted —
        // the download starts from zero and creates the sidecar.
        let body: Vec<u8> = vec![7; 32];
        let responses = Arc::new(std::sync::Mutex::new(vec![body_response(&body)]));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (port, server) = serve_scripted(responses.clone(), requests.clone());
        let root = root("no-meta");
        fs::create_dir_all(&root).unwrap();
        let artifact = root.join("artifact.bin");
        let leftover_part = artifact.with_extension("part");
        fs::write(&leftover_part, b"leftover").unwrap();

        download_resumable(
            &ArtifactSource::Http(format!("http://127.0.0.1:{port}/artifact")),
            &artifact,
            &mut |_| {},
        )
        .expect("fresh download");

        assert!(
            requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)[0]
                .contains("range=None"),
            "no Range header without a validator"
        );
        assert_eq!(fs::read(&artifact).unwrap(), body);
        // An ETag-less source cannot keep a sidecar honest — none is
        // created (#59).
        assert!(
            !part_meta_path(&leftover_part).exists(),
            "no validator means no sidecar"
        );
        drop(server);
    }
}
