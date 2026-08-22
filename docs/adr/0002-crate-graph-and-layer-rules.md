# Crate graph: domain / launch / desktop split with symmetric presentations

Status: accepted (wayfinder #20, 2026-08)

Amendment (2026-08, implemented in #25): `cellar-providers` depends on `cellar-core` in addition to the provider crates — the registry names the sealed port trait types it returns (`dyn RunnerResolver`, `dyn ManagedRunner`, `dyn WrapperContributor`), and Rust requires direct dependencies for naming. Layering intent unchanged: the registry never depends on `app`/`launch`/`storage`/`desktop`.

Cellar's workspace is a virtual Cargo workspace of flat, seam-prefixed crates with strict inward dependency rules: pure domain (`cellar-core`) at the center; infrastructure (`cellar-storage`, `cellar-desktop`, `cellar-provider-*`) implements `core` ports; launch machinery gets its own crate (`cellar-launch`); use-case orchestration (`cellar-app`) stays thin; presentations (`cellar-cli` now, `cellar-gui` later) are symmetric leaves that alone instantiate the provider registry (composition root). The CLI is primary today; the GUI becomes primary later — a product choice (default-members, packaging), not an architecture change.

## Considered options

- **Launch machinery inside `cellar-app`** (earlier research sketch): rejected — resolution outweighs orchestration; the dedicated `cellar-launch` keeps `app` thin and the machinery mock-testable in isolation.
- **Function-grouped crate names** (`cellar-runners/proton`, `cellar-runtime/umu` — the original #20 sketch): rejected — flat seam-prefixed names keep `cargo tree` greppable with only 4–6 provider crates.

## Consequences

- Adding a provider: new crate + registry line in `cellar-providers` + presentation dep — zero edits below presentation.
- Adding the GUI: one presentations leaf; nothing below it changes.
- Cycles are compile-time errors (Cargo DAG); `unsafe` forbidden workspace-wide; per-provider contract tests against the ports in `core`.