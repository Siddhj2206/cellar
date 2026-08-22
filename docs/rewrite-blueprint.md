# Cellar Rewrite Blueprint

> Deliverable of wayfinder map [Cellar rewrite blueprint (rethink)](https://github.com/Siddhj2206/cellar/issues/14) — the locked structure only; sections fill in as wayfinder tickets resolve.
> Domain vocabulary: [`CONTEXT.md`](../CONTEXT.md). Hard-to-reverse choices: [`docs/adr/`](adr/). Research notes: [`docs/research/`](research/).

## 1. Purpose & destination

<!-- What reaching the end of this blueprint looks like; what "blueprint done" means. -->

## 2. Scope

<!-- In scope / out of scope, mirroring the wayfinder map. -->

## 3. Domain model

<!-- Pointer to the CONTEXT.md glossary (locked in #15); index, never a restatement. -->

## 4. Workspace & crate graph

Locked via [#20 — Crate graph and layer rules](https://github.com/Siddhj2206/cellar/issues/20); research foundation: [#17](https://github.com/Siddhj2206/cellar/issues/17); rationale: [ADR 0002](adr/0002-crate-graph-and-layer-rules.md).

Virtual workspace (resolver 3, `members = ["crates/*"]`, workspace-level package/deps/lints inheritance — see `docs/research/rust-crate-design.md`):

```
crates/
├── cellar-core            domain: entities, ports (traits), invariants — no I/O, no platform
├── cellar-app             use-cases: FirstRunInstall / LaunchApp / DoctorCheck (orchestrates)
├── cellar-launch          launch-plan resolution: runner ref (configured → managed → PATH),
│                          wrapper chain assembly (Layer-sorted) — generic chain builder;
│                          env contracts are wrapper-provider data, not launch machinery (#21)
├── cellar-storage         infra: TOML file-tree ↔ domain mapping; sole writer of the tree (#19)
├── cellar-desktop         infra: .desktop entries, Rust-native icon extraction + cache, MIME
├── cellar-providers       registry: all_resolvers() / all_wrappers() — dep of presentations only
├── cellar-provider-proton managed GE-Proton + umu-Proton
├── cellar-provider-umu    managed umu (container launch layer)
├── cellar-provider-wine   discover-only (PATH)
├── cellar-provider-gamescope wrapper contributor
├── cellar-cli             presentation: binary `cellar` (clap) — PRIMARY now
├── cellar-gui             presentation (future) — becomes PRIMARY later; CLI stays secondary
└── xtask                  cross-crate checks, release chores (Helix/cargo pattern)
```

| Crate | Depends on |
|---|---|
| `cellar-core` | nothing (std, serde only) |
| `cellar-app`, `cellar-launch` | `core` (+ `core` ports) |
| `cellar-storage`, `cellar-desktop`, `cellar-provider-*` | `core` |
| `cellar-providers` | provider crates only |
| `cellar-cli`, `cellar-gui` | `app`, `core` DTOs, `providers` (composition root) |
| `xtask` | workspace |

**Layer rules (enforced, not aspirational):**
- Cargo's DAG — cycles are compile errors; extract shared code into `core` instead.
- `workspace.lints`: `unsafe_code = forbid`, clippy pedantic.
- Composition root lives only in presentation crates: they alone instantiate providers and inject them; nothing below presentation constructs infra.
- Generics in `app`/`launch` (`App<R: RunnerResolver>` — test with mocks); `Box<dyn _>` only at the composition root where heterogeneity is needed.
- Ports sealed, `Send + Sync`; provider additions never touch `core`/`app`/`launch`.
- Presentations are symmetric leaves: **CLI primary now, GUI primary later** — flipping primary is `default-members` + packaging, zero edits below presentation. Adding the GUI crate touches nothing else.

## 5. Provider seam: runners, wrappers, components

Locked via [#21 — Extension ports and traits](https://github.com/Siddhj2206/cellar/issues/21); foundations: [#17](https://github.com/Siddhj2206/cellar/issues/17), [#18](https://github.com/Siddhj2206/cellar/issues/18); rationale: [ADR 0003](adr/0003-extension-ports-and-platform-stance.md).

Exactly **five sealed ports** in `core::ports` — `Send + Sync`, object-safe; generics in `app`/`launch`, `Box<dyn _>` only at the composition root:

| Port | Owns | Implemented by |
|---|---|---|
| `RunnerResolver` | `resolve(&RunnerSpec) -> ResolvedRunner` — both modes; result carries the mode tag (Managed \| DiscoverOnly) and the `RunnerRef` | provider crates |
| `ManagedRunner` | managed-only lifecycle: declarative `manifest()` — source URL pattern, checksum algorithm, archive layout, install kind. Discover-only providers never implement it (no stubs) | proton, umu |
| `WrapperContributor` | layered env/behavior onto the LaunchPlan + declared `Layer`; **env contracts are wrapper data** — umu contributes the `GAMEID`/`WINEPREFIX`/`PROTONPATH`/`PROTON_VERB` contract and the `umu-run → _v2-entry-point → proton waitforexitandrun` chain | umu, gamescope, mangohud |
| `Storage` | file-tree mapping (prefix/app CRUD), discovery (`.lnk`), managed installs (shared `Installer` pipeline), XDG path resolution — one external system, one port; sole writer of the tree (#19) | cellar-storage |
| `DesktopIntegrator` | `.desktop` entries, icon extraction + cache, MIME/file associations | cellar-desktop |

**Managed vs Discover-only = trait membership.** Discover-only providers (wine via PATH, Steam Proton) implement only `RunnerResolver`; wine's `execvp` lookup and Steam compat-dir scanning are mirrors of umu's own resolution (research #18).

**Shared machinery (not ports):** the `Installer` pipeline — fetch release, verify checksum (SHA-512 for GE-Proton), extract, flock per-directory, resumable cache under `$XDG_CACHE_HOME`, `runtime/providers.toml` inventory write — is a storage-owned service driven by `ManagedRunner::manifest()`. Adding a managed runner = one descriptor, zero pipeline code. `Layer` enum in `core`: `Display → Container → RuntimeEnv`; launch sorts by it — unknown layers are compile errors.

**Not seams (deliberate — recorded so nobody "fixes" them):**
- **Platform** — no port. Linux-first; a future macOS edition enters through the existing seams only (new provider crates; per-platform storage paths via `cfg(target_os)`, a Launch Services desktop implementation) — never a `Platform` trait.
- **Install strategy** — closed three-armed logic in `app` (standalone exe / installer / archive).
- **Component** — future note: a component provider reuses the `ManagedRunner` shape when real (per #18: not prescribed now).
- **Runtime plugins** — future note: `inventory`-style link-time registry if providers ever exceed ~20 (research #17).

## 6. Storage: file-tree source of truth

Locked via [#19 — Storage layout and file format](https://github.com/Siddhj2206/cellar/issues/19); rationale: [ADR 0001](adr/0001-single-root-storage-tree.md).

Single root `$XDG_DATA_HOME/cellar/` — one movable unit, TOML everywhere:

```
cellar/
├── settings.toml          # global settings (runner resolution order, umu/proton config)
├── prefixes/
│   └── <prefix-slug>/     # user-chosen name at InstallSession, default `default`
│       └── prefix.toml    # prefix-level defaults (runner, env, graphics, Windows version)
├── apps/
│   └── <app-slug>.toml    # one per AppEntry: exe path, kind, overrides (incl. prefix
│                          #   binding), runner ref, current-state metadata
├── runtime/               # managed provider installs (GE-Proton, umu) + providers.toml
│                          #   inventory — authoritative, re-installable
└── cache/                 # disposable: downloads, icons, discovery results
```

- **Format**: TOML (spec v1.1.0, `toml` crate) — purpose-built human-edited config with comments; YAML/JSON rejected (ADR 0001).
- **Naming**: human-readable slugs, `-2` dedupe; file name = entry's display name; identity stays the exe path (renaming renames the file).
- **Robustness**: `schema_version` per file; migrations are file-tree transforms; atomic writes (temp + rename), app service the sole writer; invalid files degrade — entry skipped, flagged by `cellar doctor`, never silently overwritten.
- **Ownership**: delete-prefix removes its dir, uninstall removes its app file — never more. `cache/` re-derivable at any time.

## 7. Launch pipeline

Locked via [#22 — Launch pipeline model](https://github.com/Siddhj2206/cellar/issues/22); foundations: #15 (LaunchPlan/Override/RunnerRef terms), #18 (umu chain), #20 (`cellar-launch` crate), #21 (ports, `Layer` enum, env contracts as wrapper data).

**Four phases — nothing spawns until the plan is complete:**

```
resolve → check → plan → execute
```

1. **Resolve** — two-stage precedence (below) yields the concrete `RunnerRef`, prefix path, and effective settings.
2. **Check** — per-launch pre-flight on exactly this launch's dependencies: exe exists, prefix exists, runner install intact, wrapper runtimes present (umu's SLR). The same checks swept across everything = `doctor`; the check phase is doctor applied to one launch.
3. **Plan** — build the LaunchPlan as a **pure, printable value** (`Debug` + `serde` + human render): final `argv`, env contract, cwd, wrapper chain (Layer-sorted, #21). Nothing spawns; `--dry-run` and GUI preview are free features, and the printed `argv` is a user-runnable reproduction for bug reports.
4. **Execute** — spawn from the frozen plan → `SpawnedProcess` handle (pid, log path, `wait()`).

**Two-stage precedence:**
- **Selection** (which runner family): app override → prefix default → defaults floor (`settings.toml` + kind presets — Game → GE-Proton, Tool → wine). The prefix-binding override decides *which* prefix's defaults apply at all.
- **Resolution** (spec → concrete ref): configured path → managed install → PATH (research #18).

**Canonical stacks** (examples, not exhaustive):
- **Managed Proton**: `gamescope? → umu-run (Container; runs the SLR container internally, per #18) → proton waitforexitandrun → exe`; umu contributes `GAMEID`/`WINEPREFIX`/`PROTONPATH`/`PROTON_VERB`.
- **Plain wine**: `wine <exe>` — **co-equal chain selected by config** (Tools / explicit override), *never an automatic fallback* for a failed Proton selection; a failed Proton resolve is a doctor-flagged error with a suggested fix (#18).
- **Env assembly precedence**: app env overrides → wrapper contributions → prefix env → base.

**Execute semantics:** InstallSession always awaits the artifact's exit (the installer must finish before discovery); LaunchApp's wait-vs-detach is presentation policy (CLI foregrounds, GUI detaches — surface detail lands with #23). Game output always goes to `cache/launch-logs/<slug>-<timestamp>.log` (disposable, #19); CLI may forward it, GUI may tail it.

**Failure taxonomy** — five families, cut before spawn:

| Family | Caught | Disposition |
|---|---|---|
| Resolve | pre-flight | unknown spec / order exhausted → doctor: SuggestInstall |
| Check | pre-flight | exe missing → re-register; prefix missing → recreate; runner corrupt → reinstall |
| Plan | pre-flight | wrapper contribution failure (e.g. missing SLR runtime) → doctor: install runtime |
| Spawn | post-plan | OS exec error, reported as-is |
| Runtime | post-spawn | exit code propagated raw — no magic mapping |

## 8. CLI surface & first-run UX

<!-- Pending #23. -->

## 9. Errors & doctor

<!-- Fog: error/doctor capability model and exit codes. -->

## 10. Testing strategy

<!-- Fog: multi-crate workspace testing strategy. -->

## 11. ADR index

<!-- Links to docs/adr/ entries for hard-to-reverse choices. -->