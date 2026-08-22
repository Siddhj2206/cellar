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

<!-- Locked: virtual workspace, four-layer DAG (research #17). Details land as #20 resolves. -->

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