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
│                          umu env contract (GAMEID/WINEPREFIX/PROTONPATH/PROTON_VERB),
│                          wrapper layering — generic over ports, mock-testable in isolation
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

<!-- Locked: compile-time provider crates, Managed vs Discover-only modes (research #17/#18). Details land as #21 resolves. -->

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

<!-- Pending #22. -->

## 8. CLI surface & first-run UX

<!-- Pending #23. -->

## 9. Errors & doctor

<!-- Fog: error/doctor capability model and exit codes. -->

## 10. Testing strategy

<!-- Fog: multi-crate workspace testing strategy. -->

## 11. ADR index

<!-- Links to docs/adr/ entries for hard-to-reverse choices. -->